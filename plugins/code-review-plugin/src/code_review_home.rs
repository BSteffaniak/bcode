//! Code review workspace home/picker TUI.

use bcode_plugin_sdk::path::display_from_current_dir;
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
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface, PluginTuiTheme};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    ChildLayout, Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStateStyles};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListComponent, SelectableListItem, SelectableListOutcome,
    SelectableListState, SelectableListStyles,
};
use tokio::sync::mpsc;

use crate::tui_host_types::TuiError;

#[derive(Debug, Clone, Copy)]
struct ReviewHomeTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    selection: Style,
}

impl ReviewHomeTheme {
    fn resolve(theme: Option<&PluginTuiTheme>) -> Self {
        theme.map_or_else(
            || Self {
                canvas: Style::new(),
                text: Style::new(),
                muted: Style::new().add_modifier(Modifier::DIM),
                focused: Style::new().add_modifier(Modifier::BOLD),
                selection: Style::new().add_modifier(Modifier::REVERSED),
            },
            |theme| Self {
                canvas: theme.canvas,
                text: theme.text,
                muted: theme.muted,
                focused: theme.focused,
                selection: theme.selection,
            },
        )
    }
}

/// Review home outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::large_enum_variant)]
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

/// Caller-owned BMUX state and the exact item snapshot/rectangle last painted.
#[derive(Debug, Clone)]
struct HomeList {
    id: &'static str,
    items: Vec<SelectableListItem>,
    state: SelectableListState,
    painted_area: Option<Rect>,
    painted_layout: Option<LayoutNode>,
}

impl HomeList {
    const fn new(id: &'static str) -> Self {
        Self {
            id,
            items: Vec::new(),
            state: SelectableListState::new(None),
            painted_area: None,
            painted_layout: None,
        }
    }

    fn paint(
        &mut self,
        items: Vec<SelectableListItem>,
        area: Rect,
        selected: Option<usize>,
        frame: &mut PaintCx<'_, '_>,
        theme: ReviewHomeTheme,
    ) {
        let selection_changed = self.items != items || self.state.selected() != selected;
        if self.items != items {
            self.state = SelectableListState::new(selected);
        } else if self.state.selected() != selected {
            self.state.set_selected(selected);
            self.state.set_focused(selected);
        }
        self.items = items;
        self.painted_area = (!area.is_empty()).then_some(area);
        let list = SelectableList::new(&self.items).styles(home_list_styles(theme));
        list.clamp_state(area, &mut self.state);
        let state = std::cell::Cell::new(self.state);
        let component = SelectableListComponent::new(self.id, &self.items, &state)
            .styles(home_list_styles(theme))
            .fallback_style(theme.text);
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        // Do not snap back to focus after wheel scrolling; reveal only semantic selection changes.
        if selection_changed {
            component.reveal_focus(&layout);
        }
        self.state = state.get();
        frame.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::terminal(Rect::new(0, 0, area.width, area.height)),
            |child| component.paint(&layout, child),
        );
        self.painted_layout = Some(LayoutNode::with_children(
            LayoutId::new("home-list-root"),
            LogicalSize::new(u64::from(area.right()), u64::from(area.bottom())),
            vec![ChildLayout::new(
                u64::from(area.x),
                u64::from(area.y),
                layout,
            )],
        ));
    }

    fn event(&mut self, event: &Event) -> SelectableListOutcome {
        let Some(area) = self.painted_area else {
            return SelectableListOutcome::Ignored;
        };
        let Some(root) = self.painted_layout.as_ref() else {
            return SelectableListOutcome::Ignored;
        };
        let event = if matches!(event, Event::Mouse(mouse) if !area.contains(mouse.position) && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Up(_)))
        {
            &Event::Focus(bmux_tui::event::FocusEvent::Lost)
        } else {
            event
        };
        let prior_selected = self.state.selected();
        // BMUX list selection is value-oriented. Home rows are repeatable actions:
        // releasing the already-selected row must still resume/create it.
        if matches!(event, Event::Mouse(mouse) if matches!(mouse.kind, bmux_tui::event::MouseEventKind::Up(_)))
        {
            self.state.set_selected(None);
        }
        let state = std::cell::Cell::new(self.state);
        let component = SelectableListComponent::new(self.id, &self.items, &state);
        let mut cx = EventCx::with_clip(root, area);
        let child = &root.children[0];
        let outcome = cx.with_child(child, |cx| component.handle_event(event, &child.node, cx));
        self.state = state.get();
        if !matches!(outcome, SelectableListOutcome::Selected(_)) {
            self.state.set_selected(prior_selected);
        }
        outcome
    }
}

fn home_list_styles(theme: ReviewHomeTheme) -> SelectableListStyles {
    SelectableListStyles {
        background: theme.canvas,
        normal: theme.text,
        focused: theme.focused,
        selected: theme.selection,
        hovered: theme.focused,
        pressed: theme.selection,
        disabled: theme.muted,
        ..SelectableListStyles::default()
    }
}

const HOME_PRESETS: [(&str, char, &str); 6] = [
    ("Unstaged changes", 'u', "Tracked edits not staged yet"),
    ("Staged changes", 's', "The index, ready to commit"),
    ("Worktree + staged", 'w', "All tracked local changes"),
    ("Repository", 'v', "Browse and review repository files"),
    ("Last commit", 'l', "Review the most recent commit"),
    (
        "Custom review…",
        'n',
        "Name a workspace, then choose sources in Build",
    ),
];

fn home_effective_hints(scope: &str, keys: impl Fn(&str) -> Vec<String>) -> Vec<String> {
    use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
    let mut hints = crate::keybindings::effective_hints(scope, &keys);
    if scope == crate::keybindings::HOME {
        for control in [
            ControlAction::FocusNext,
            ControlAction::FocusPrevious,
            ControlAction::Cancel,
        ] {
            let definition = control.definition(scope);
            let bindings = keys(&definition.id);
            if !bindings.is_empty() {
                hints.push(format!("{} {}", bindings.join("/"), definition.description));
            }
        }
    }
    hints
}

fn home_preset_hint(key: char) -> String {
    let Some(action) = crate::keybindings::actions_for(crate::keybindings::HOME)
        .into_iter()
        .find(|action| {
            action
                .defaults
                .iter()
                .any(|binding| binding == &key.to_string())
        })
    else {
        return String::new();
    };
    crate::keybindings::hints()
        .unwrap_or_default()
        .into_iter()
        .find_map(|hint| {
            hint.strip_suffix(&action.description)
                .map(|keys| format!("  [{}]", keys.trim()))
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
struct ReviewHomeApp {
    help: HomeList,
    presets: HomeList,
    workspaces: HomeList,
    preset_focus: bool,
    preset_selected: usize,
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
    attention_filter_active: bool,
    help_visible: bool,
    should_exit: bool,
    outcome: Option<ReviewHomeOutcome>,
}

const fn keymap_scope(app: &ReviewHomeApp) -> &'static str {
    if app.new_review_buffer.is_some() {
        crate::keybindings::NEW
    } else if app.rename_buffer.is_some() {
        crate::keybindings::RENAME
    } else if app.search_active {
        crate::keybindings::SEARCH
    } else {
        crate::keybindings::HOME
    }
}

impl ReviewHomeApp {
    const fn new(repo_path: PathBuf, workspace_items: Vec<ReviewWorkspaceListItem>) -> Self {
        Self {
            help: HomeList::new("review-home-help"),
            presets: HomeList::new("review-home-presets"),
            workspaces: HomeList::new("review-home-workspaces"),
            preset_focus: false,
            preset_selected: 0,
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
            attention_filter_active: false,
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
                if self.attention_filter_active && !workspace_needs_attention(item) {
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

    #[cfg(test)]
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

    /// Keep the current semantic selection when asynchronous work reorders rows.
    fn reconcile_selection(&mut self, workspace_id: Option<&str>) {
        if let Some(selected) = workspace_id.and_then(|id| {
            self.visible_indices().into_iter().position(|index| {
                self.workspace(index)
                    .is_some_and(|workspace| workspace.id == id)
            })
        }) {
            self.selected = selected;
        } else {
            self.clamp_selection();
        }
    }

    fn selected_workspace(&self) -> Option<ReviewWorkspace> {
        self.selected_workspace_index()
            .and_then(|index| self.workspace(index).cloned())
    }

    fn selected_workspace_or_status(&mut self) -> Option<ReviewWorkspace> {
        let Some(workspace) = self.selected_workspace() else {
            self.status_message = Some(
                "no matching review workspace selected; choose Custom review to create one"
                    .to_string(),
            );
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
            self.status_message = Some("review is archived; restore it before opening".to_string());
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
            self.status_message = Some(
                "no matching review workspace selected; choose Custom review to create one"
                    .to_string(),
            );
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

    fn select_review_by_attention(&mut self, forward: bool) -> bool {
        self.select_review_by_predicate("attention", forward, workspace_needs_attention)
    }

    fn select_review_by_health(&mut self, target: &str, forward: bool) -> bool {
        self.select_review_by_predicate(target, forward, |item| {
            workspace_health_label(item) == target
        })
    }

    fn select_review_by_predicate(
        &mut self,
        label: &str,
        forward: bool,
        predicate: impl Fn(&ReviewWorkspaceListItem) -> bool,
    ) -> bool {
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
            if self.workspace_item(workspace_index).is_some_and(&predicate) {
                self.selected = selected;
                self.status_message = Some(format!("selected {label} review"));
                return true;
            }
        }
        self.status_message = Some(format!("no {label} reviews in the current filter"));
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

    fn select_next_attention_review(&mut self) -> bool {
        self.select_review_by_attention(true)
    }

    fn select_previous_attention_review(&mut self) -> bool {
        self.select_review_by_attention(false)
    }

    fn start_new_review(&mut self) -> bool {
        self.new_review_buffer = Some(String::new());
        self.status_message =
            Some("new review title; submitting an empty title uses Untitled review".to_string());
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

    fn toggle_attention_filter(&mut self) -> bool {
        self.attention_filter_active = !self.attention_filter_active;
        self.clamp_selection();
        self.status_message = Some(if self.attention_filter_active {
            "showing reviews needing attention".to_string()
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

pub struct ReviewHomeSurface {
    client: BcodeClient,
    app: ReviewHomeApp,
    pending_action: Option<ReviewHomeAsyncAction>,
    action_sender: mpsc::UnboundedSender<ReviewHomeAsyncResult>,
    action_receiver: mpsc::UnboundedReceiver<ReviewHomeAsyncResult>,
}

#[derive(Debug, Clone)]
enum ReviewHomeAsyncAction {
    ArchiveSelectedWorkspace {
        workspace_id: String,
    },
    CreatePreset {
        title: String,
        source_kinds: Vec<ReviewSourceKind>,
    },
    ToggleArchived {
        include_archived: bool,
    },
    RestoreSelectedWorkspace {
        workspace: ReviewWorkspace,
    },
    SubmitNewReview {
        title: String,
    },
    SubmitRename {
        workspace: ReviewWorkspace,
    },
}

#[derive(Debug)]
enum ReviewHomeAsyncResult {
    ArchiveSelectedWorkspace(Result<ArchiveSelectedWorkspaceResult, TuiError>),
    CreatePreset(Result<ReviewWorkspace, TuiError>),
    ToggleArchived(Result<ToggleArchivedResult, TuiError>),
    RestoreSelectedWorkspace(Result<ReviewWorkspace, TuiError>),
    SubmitNewReview(Result<ReviewWorkspace, TuiError>),
    SubmitRename(Result<ReviewWorkspace, TuiError>),
}

#[derive(Debug)]
struct ArchiveSelectedWorkspaceResult {
    workspace_id: String,
    archived: bool,
}

#[derive(Debug)]
struct ToggleArchivedResult {
    include_archived: bool,
    workspaces: Vec<ReviewWorkspaceListItem>,
}

impl ReviewHomeSurface {
    /// Load the review home surface.
    ///
    /// # Errors
    ///
    /// Returns an error when initial workspace loading fails catastrophically.
    pub async fn load(repo_path: PathBuf) -> Result<Self, TuiError> {
        let client = BcodeClient::default_endpoint();
        let app = match load_workspaces(&client, repo_path.clone()).await {
            Ok(workspaces) => ReviewHomeApp::new(repo_path, workspaces),
            Err(error) => {
                let mut app = ReviewHomeApp::new(repo_path, Vec::new());
                app.status_message = Some(format!("failed to load workspaces: {error}"));
                app
            }
        };
        let (action_sender, action_receiver) = mpsc::unbounded_channel();
        Ok(Self {
            client,
            app,
            pending_action: None,
            action_sender,
            action_receiver,
        })
    }

    fn action_after_state_update(&mut self, needs_redraw: bool) -> PluginTuiAction {
        if self.app.should_exit {
            let outcome = self.app.outcome.take().and_then(|outcome| {
                serde_json::to_value(outcome)
                    .ok()
                    .map(|value| serde_json::json!({ "review_home": value }))
            });
            PluginTuiAction::Close { outcome }
        } else if needs_redraw {
            PluginTuiAction::Redraw
        } else {
            PluginTuiAction::None
        }
    }

    fn spawn_pending_action(&mut self, host: &dyn PluginTuiHost) {
        let Some(action) = self.pending_action.take() else {
            return;
        };
        let client = self.client.clone();
        let repo_path = self.app.repo_path.clone();
        let sender = self.action_sender.clone();
        host.spawn(Box::pin(async move {
            let result = run_home_async_action(&client, repo_path, action).await;
            let _ = sender.send(result);
        }));
    }
}

async fn run_home_async_action(
    client: &BcodeClient,
    repo_path: PathBuf,
    action: ReviewHomeAsyncAction,
) -> ReviewHomeAsyncResult {
    match action {
        ReviewHomeAsyncAction::ArchiveSelectedWorkspace { workspace_id } => {
            ReviewHomeAsyncResult::ArchiveSelectedWorkspace(
                archive_workspace_action(client, repo_path, workspace_id).await,
            )
        }
        ReviewHomeAsyncAction::CreatePreset {
            title,
            source_kinds,
        } => ReviewHomeAsyncResult::CreatePreset(
            create_workspace_with_sources(client, repo_path, title, source_kinds).await,
        ),
        ReviewHomeAsyncAction::ToggleArchived { include_archived } => {
            ReviewHomeAsyncResult::ToggleArchived(
                load_workspaces_with_archived(client, repo_path, include_archived)
                    .await
                    .map(|workspaces| ToggleArchivedResult {
                        include_archived,
                        workspaces,
                    }),
            )
        }
        ReviewHomeAsyncAction::RestoreSelectedWorkspace { workspace } => {
            ReviewHomeAsyncResult::RestoreSelectedWorkspace(
                update_workspace_action(client, repo_path, workspace).await,
            )
        }
        ReviewHomeAsyncAction::SubmitNewReview { title } => ReviewHomeAsyncResult::SubmitNewReview(
            create_workspace_with_sources(client, repo_path, title, Vec::new()).await,
        ),
        ReviewHomeAsyncAction::SubmitRename { workspace } => ReviewHomeAsyncResult::SubmitRename(
            update_workspace_action(client, repo_path, workspace).await,
        ),
    }
}

fn apply_home_async_result(app: &mut ReviewHomeApp, result: ReviewHomeAsyncResult) {
    match result {
        ReviewHomeAsyncResult::ArchiveSelectedWorkspace(result) => match result {
            Ok(result) => apply_archive_selected_workspace_result(app, &result),
            Err(error) => {
                app.status_message = Some(format!("failed to archive workspace: {error}"));
            }
        },
        ReviewHomeAsyncResult::CreatePreset(result) => match result {
            Ok(workspace) => {
                app.workspace_items.push(ReviewWorkspaceListItem {
                    workspace: workspace.clone(),
                    thread_count: 0,
                    draft_count: 0,
                    last_publish: None,
                });
                sort_workspace_items_recent_first(&mut app.workspace_items);
                app.open_workspace(workspace, false);
            }
            Err(error) => {
                app.status_message = Some(format!("failed to create workspace: {error}"));
            }
        },
        ReviewHomeAsyncResult::ToggleArchived(result) => match result {
            Ok(result) => {
                let selected_id = app.selected_workspace().map(|workspace| workspace.id);
                app.include_archived = result.include_archived;
                app.workspace_items = result.workspaces;
                app.reconcile_selection(selected_id.as_deref());
                app.status_message = Some(if app.include_archived {
                    "showing archived reviews".to_string()
                } else {
                    "hiding archived reviews".to_string()
                });
            }
            Err(error) => {
                app.status_message = Some(format!("failed to toggle archived reviews: {error}"));
            }
        },
        ReviewHomeAsyncResult::RestoreSelectedWorkspace(result) => match result {
            Ok(workspace) => apply_restored_workspace(app, workspace),
            Err(error) => {
                app.status_message = Some(format!("failed to restore workspace: {error}"));
            }
        },
        ReviewHomeAsyncResult::SubmitNewReview(result) => match result {
            Ok(workspace) => {
                app.open_workspace(workspace, true);
            }
            Err(error) => {
                app.status_message = Some(format!("failed to create workspace: {error}"));
            }
        },
        ReviewHomeAsyncResult::SubmitRename(result) => match result {
            Ok(workspace) => apply_renamed_workspace(app, workspace),
            Err(error) => {
                app.status_message = Some(format!("failed to rename workspace: {error}"));
            }
        },
    }
}

fn apply_archive_selected_workspace_result(
    app: &mut ReviewHomeApp,
    result: &ArchiveSelectedWorkspaceResult,
) {
    if !result.archived {
        app.status_message = Some("workspace was not found".to_string());
        return;
    }
    let Some(index) = app
        .workspace_items
        .iter()
        .position(|item| item.workspace.id == result.workspace_id)
    else {
        app.status_message = Some("archived workspace".to_string());
        return;
    };
    let selected_id = app.selected_workspace().map(|workspace| workspace.id);
    let archived = app.workspace_items.remove(index);
    app.reconcile_selection(selected_id.as_deref());
    app.status_message = Some(format!("archived {}", archived.workspace.title));
}

fn apply_restored_workspace(app: &mut ReviewHomeApp, workspace: ReviewWorkspace) {
    let selected_id = app.selected_workspace().map(|workspace| workspace.id);
    if let Some(item) = app
        .workspace_items
        .iter_mut()
        .find(|item| item.workspace.id == workspace.id)
    {
        item.workspace = workspace;
    }
    app.reconcile_selection(selected_id.as_deref());
    app.status_message = Some("restored review workspace".to_string());
}

fn apply_renamed_workspace(app: &mut ReviewHomeApp, workspace: ReviewWorkspace) {
    let selected_id = app.selected_workspace().map(|workspace| workspace.id);
    let new_title = workspace.title.clone();
    if let Some(item) = app
        .workspace_items
        .iter_mut()
        .find(|item| item.workspace.id == workspace.id)
    {
        item.workspace = workspace;
    }
    app.reconcile_selection(selected_id.as_deref());
    app.status_message = Some(format!("renamed review to {new_title}"));
}

impl PluginTuiSurface for ReviewHomeSurface {
    fn id(&self) -> &'static str {
        "code-review-home"
    }

    fn title(&self) -> &'static str {
        "Code Review Home"
    }

    fn render(&mut self, _area: Rect, frame: &mut PaintCx<'_, '_>) {
        render(&mut self.app, frame, ReviewHomeTheme::resolve(None));
    }

    fn render_with_theme(
        &mut self,
        _area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        render(&mut self.app, frame, ReviewHomeTheme::resolve(theme));
    }

    fn render_with_host(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
        host: &dyn PluginTuiHost,
    ) {
        let scope = keymap_scope(&self.app);
        crate::keybindings::set_hints(home_effective_hints(scope, |action| {
            host.keymap_keys(scope, action)
        }));
        self.render_with_theme(area, frame, theme);
        crate::keybindings::clear_hints();
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let Event::Key(key) = event else {
            let (redraw, action) = home_pointer_event(&mut self.app, event);
            let redraw = action.map_or(redraw, |key| {
                handle_plugin_key_event(&mut self.app, key, &mut self.pending_action)
            });
            self.spawn_pending_action(host);
            return self.action_after_state_update(redraw);
        };
        let scope = keymap_scope(&self.app);
        let Some(key) =
            crate::keybindings::from_host(host, scope, *key, scope != crate::keybindings::HOME)
        else {
            return PluginTuiAction::None;
        };
        let needs_redraw = handle_plugin_key_event(&mut self.app, key, &mut self.pending_action);
        self.spawn_pending_action(host);
        self.action_after_state_update(needs_redraw)
    }

    fn poll(&mut self, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let mut needs_redraw = false;
        while let Ok(result) = self.action_receiver.try_recv() {
            apply_home_async_result(&mut self.app, result);
            needs_redraw = true;
        }
        self.action_after_state_update(needs_redraw)
    }
}

fn home_pointer_event(app: &mut ReviewHomeApp, event: &Event) -> (bool, Option<KeyStroke>) {
    if matches!(event, Event::Resize(_)) {
        app.help.event(event);
        app.presets.event(event);
        app.workspaces.event(event);
        app.help.painted_area = None;
        app.presets.painted_area = None;
        app.workspaces.painted_area = None;
        return (true, None);
    }
    if matches!(event, Event::Focus(_)) {
        app.help.event(event);
        app.presets.event(event);
        app.workspaces.event(event);
        return (true, None);
    }
    if app.help_visible {
        return (
            app.help.event(event) != SelectableListOutcome::Ignored,
            None,
        );
    }
    if app.search_active || app.rename_buffer.is_some() || app.new_review_buffer.is_some() {
        return (false, None);
    }
    let Event::Mouse(mouse) = event else {
        return (false, None);
    };
    let on_presets = app
        .presets
        .painted_area
        .is_some_and(|area| area.contains(mouse.position));
    let on_workspaces = app
        .workspaces
        .painted_area
        .is_some_and(|area| area.contains(mouse.position));
    // A release outside must still clear the component's pressed state.
    let preset = app.presets.event(event);
    let workspace = app.workspaces.event(event);
    if on_presets && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_)) {
        app.preset_focus = true;
    }
    if on_workspaces && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_)) {
        app.preset_focus = false;
    }
    match preset {
        SelectableListOutcome::Focused(index) => app.preset_selected = index,
        SelectableListOutcome::Selected(index) => {
            app.preset_selected = index;
            if let Some((_, key, _)) = HOME_PRESETS.get(index) {
                return (true, Some(KeyStroke::simple(KeyCode::Char(*key))));
            }
        }
        _ => {}
    }
    match workspace {
        SelectableListOutcome::Focused(index) => app.selected = index,
        SelectableListOutcome::Selected(index) => {
            // Resolve against stable painted identity, never a newly filtered positional index.
            let id = app.workspaces.items.get(index).map(|item| item.id.as_str());
            if let Some(selected) = app.visible_indices().iter().position(|index| {
                app.workspace(*index)
                    .is_some_and(|workspace| Some(workspace.id.as_str()) == id)
            }) {
                app.selected = selected;
                return (true, Some(KeyStroke::simple(KeyCode::Enter)));
            }
        }
        _ => {}
    }
    (
        preset != SelectableListOutcome::Ignored
            || workspace != SelectableListOutcome::Ignored
            || on_presets
            || on_workspaces,
        None,
    )
}

/// Navigation is adapted only after effective keymap resolution.
fn home_navigation(app: &mut ReviewHomeApp, key: KeyCode) -> Option<KeyCode> {
    if app.search_active || app.rename_buffer.is_some() || app.new_review_buffer.is_some() {
        return Some(key);
    }
    if app.help_visible {
        let navigation = match key {
            KeyCode::Char('j') => KeyCode::Down,
            KeyCode::Char('k') => KeyCode::Up,
            key => key,
        };
        if matches!(
            navigation,
            KeyCode::Down
                | KeyCode::Up
                | KeyCode::PageDown
                | KeyCode::PageUp
                | KeyCode::Home
                | KeyCode::End
        ) {
            app.help.event(&Event::Key(KeyStroke::simple(navigation)));
            return None;
        }
        if key == KeyCode::Escape {
            app.help_visible = false;
            return None;
        }
        return (key == KeyCode::Char('?')).then_some(key);
    }
    if key == KeyCode::Tab {
        app.preset_focus = !app.preset_focus;
        return None;
    }
    if app.preset_focus {
        match key {
            KeyCode::Down | KeyCode::Char('j') => {
                app.preset_selected = (app.preset_selected + 1).min(HOME_PRESETS.len() - 1);
                return None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.preset_selected = app.preset_selected.saturating_sub(1);
                return None;
            }
            KeyCode::Home | KeyCode::Char('g') => {
                app.preset_selected = 0;
                return None;
            }
            KeyCode::End | KeyCode::Char('G') => {
                app.preset_selected = HOME_PRESETS.len() - 1;
                return None;
            }
            KeyCode::Enter => return Some(KeyCode::Char(HOME_PRESETS[app.preset_selected].1)),
            _ => {}
        }
    }
    Some(key)
}

fn handle_plugin_key_event(
    app: &mut ReviewHomeApp,
    stroke: KeyStroke,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    if is_review_home_exit_key(stroke) {
        app.outcome = Some(ReviewHomeOutcome::Exit);
        app.should_exit = true;
        return true;
    }
    if stroke.modifiers.ctrl
        || stroke.modifiers.alt
        || stroke.modifiers.super_key
        || stroke.modifiers.hyper
        || stroke.modifiers.meta
    {
        return false;
    }
    let key = normalized_home_key(stroke);
    if pending_action.is_some() {
        app.status_message = Some("review action already in progress".to_string());
        return true;
    }
    if app.new_review_buffer.is_some() {
        return handle_plugin_new_review_key(app, key, pending_action);
    }
    if app.rename_buffer.is_some() {
        return handle_plugin_rename_key(app, key, pending_action);
    }
    if app.search_active {
        return app.handle_search_key(key);
    }
    handle_plugin_normal_key(app, key, pending_action)
}

fn handle_plugin_new_review_key(
    app: &mut ReviewHomeApp,
    key: KeyCode,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    match app.handle_new_review_key(key) {
        ReviewHomeKeyOutcome::Redraw => true,
        ReviewHomeKeyOutcome::SubmitNewReview => {
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
            *pending_action = Some(ReviewHomeAsyncAction::SubmitNewReview { title });
            app.status_message = Some("creating review workspace...".to_string());
            true
        }
        ReviewHomeKeyOutcome::SubmitRename | ReviewHomeKeyOutcome::Ignored => false,
    }
}

fn handle_plugin_rename_key(
    app: &mut ReviewHomeApp,
    key: KeyCode,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    match app.handle_rename_key(key) {
        ReviewHomeKeyOutcome::Redraw => true,
        ReviewHomeKeyOutcome::SubmitRename => queue_rename_selected_workspace(app, pending_action),
        ReviewHomeKeyOutcome::SubmitNewReview | ReviewHomeKeyOutcome::Ignored => false,
    }
}

fn handle_plugin_normal_key(
    app: &mut ReviewHomeApp,
    key: KeyCode,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    let Some(key) = home_navigation(app, key) else {
        return true;
    };
    match key {
        KeyCode::Char('/') => app.toggle_search(),
        KeyCode::Char('r') => app.start_rename(),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char('d') => {
            app.details_visible = !app.details_visible;
            true
        }
        KeyCode::Char('D') => app.toggle_draft_filter(),
        KeyCode::Char('A') => app.toggle_attention_filter(),
        KeyCode::Char('?') => {
            app.help_visible = !app.help_visible;
            true
        }
        KeyCode::Enter => app.open_selected(),
        KeyCode::Char('c') => app.open_most_recent(),
        KeyCode::Char('o') => app.open_selected_in_review_mode(),
        KeyCode::Char('b') => app.open_selected_in_build_mode(),
        KeyCode::Char('x') => queue_archive_selected_workspace(app, pending_action),
        KeyCode::Char('n' | 'e') => app.start_new_review(),
        KeyCode::Char('u' | 's' | 'w' | 'l' | 'v') => {
            queue_create_preset_for_key(app, key, pending_action)
        }
        KeyCode::Char('S') => app.select_next_setup_review(),
        KeyCode::Char('U') => app.select_previous_setup_review(),
        KeyCode::Char('F') => app.select_next_draft_review(),
        KeyCode::Char('B') => app.select_previous_draft_review(),
        KeyCode::Char('p') => app.select_next_published_review(),
        KeyCode::Char('P') => app.select_previous_published_review(),
        KeyCode::Char('M') => app.select_next_attention_review(),
        KeyCode::Char('N') => app.select_previous_attention_review(),
        KeyCode::Char('a') => queue_toggle_archived(app, pending_action),
        KeyCode::Char('g') => {
            app.selected = 0;
            true
        }
        KeyCode::Char('G') => {
            app.selected = app.visible_indices().len().saturating_sub(1);
            true
        }
        KeyCode::Char('R') => queue_restore_selected_workspace(app, pending_action),
        _ => false,
    }
}

fn queue_create_preset_for_key(
    app: &mut ReviewHomeApp,
    key: KeyCode,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
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
    *pending_action = Some(ReviewHomeAsyncAction::CreatePreset {
        title: title.to_string(),
        source_kinds: vec![source_kind],
    });
    app.status_message = Some("creating review workspace...".to_string());
    true
}

fn queue_toggle_archived(
    app: &mut ReviewHomeApp,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    let include_archived = !app.include_archived;
    *pending_action = Some(ReviewHomeAsyncAction::ToggleArchived { include_archived });
    app.status_message = Some("loading review workspaces...".to_string());
    true
}

fn queue_archive_selected_workspace(
    app: &mut ReviewHomeApp,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return true;
    };
    let Some(workspace) = app.workspace(workspace_index) else {
        app.status_message = Some("no review workspace selected".to_string());
        return true;
    };
    *pending_action = Some(ReviewHomeAsyncAction::ArchiveSelectedWorkspace {
        workspace_id: workspace.id.clone(),
    });
    app.status_message = Some(format!("archiving {}...", workspace.title));
    true
}

fn queue_restore_selected_workspace(
    app: &mut ReviewHomeApp,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return true;
    };
    let Some(existing) = app.workspace(workspace_index) else {
        app.status_message = Some("no review workspace selected".to_string());
        return true;
    };
    if existing.archived_at_ms.is_none() {
        app.status_message = Some("selected review is not archived".to_string());
        return true;
    }
    let mut workspace = existing.clone();
    workspace.archived_at_ms = None;
    *pending_action = Some(ReviewHomeAsyncAction::RestoreSelectedWorkspace { workspace });
    app.status_message = Some("restoring review workspace...".to_string());
    true
}

fn queue_rename_selected_workspace(
    app: &mut ReviewHomeApp,
    pending_action: &mut Option<ReviewHomeAsyncAction>,
) -> bool {
    let Some(new_title) = app.rename_buffer.take() else {
        return false;
    };
    let new_title = new_title.trim().to_string();
    if new_title.is_empty() {
        app.status_message = Some("review title cannot be empty".to_string());
        return true;
    }
    let Some(workspace_index) = app.selected_workspace_index() else {
        app.status_message = Some("no matching review workspace selected".to_string());
        return true;
    };
    let Some(existing) = app.workspace(workspace_index) else {
        app.status_message = Some("selected review workspace is unavailable".to_string());
        return true;
    };
    let mut workspace = existing.clone();
    workspace.title = new_title;
    *pending_action = Some(ReviewHomeAsyncAction::SubmitRename { workspace });
    app.status_message = Some("renaming review workspace...".to_string());
    true
}

fn is_review_home_exit_key(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Char('d')
        && stroke.modifiers.ctrl
        && !stroke.modifiers.alt
        && !stroke.modifiers.shift
        && !stroke.modifiers.super_key
        && !stroke.modifiers.hyper
        && !stroke.modifiers.meta
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

async fn archive_workspace_action(
    client: &BcodeClient,
    repo_path: PathBuf,
    workspace_id: String,
) -> Result<ArchiveSelectedWorkspaceResult, TuiError> {
    let payload = serde_json::to_vec(&ArchiveReviewWorkspaceRequest {
        repo_path,
        workspace_id: workspace_id.clone(),
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
    Ok(ArchiveSelectedWorkspaceResult {
        workspace_id,
        archived: response.archived,
    })
}

async fn update_workspace_action(
    client: &BcodeClient,
    repo_path: PathBuf,
    workspace: ReviewWorkspace,
) -> Result<ReviewWorkspace, TuiError> {
    let payload = serde_json::to_vec(&UpdateReviewWorkspaceRequest {
        repo_path,
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
    Ok(response.workspace)
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

fn render(app: &mut ReviewHomeApp, frame: &mut PaintCx<'_, '_>, theme: ReviewHomeTheme) {
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    app.help.painted_area = None;
    app.presets.painted_area = None;
    app.workspaces.painted_area = None;
    if area.is_empty() {
        return;
    }
    frame.fill(LocalRect::terminal(area), " ", theme.canvas);
    if app.help_visible {
        render_help(app, area, frame, theme);
        return;
    }
    render_header(app, area, frame, theme);
    let body_y = area.height.min(2);
    let body_height = area.height.saturating_sub(body_y).saturating_sub(2);
    // Fixed presentation split only; BMUX owns item measurement and scrolling.
    let preset_height = body_height.min(if body_height >= 10 {
        7
    } else {
        body_height / 2
    });
    if preset_height > 0 {
        let items = HOME_PRESETS
            .iter()
            .map(|(label, key, _)| {
                SelectableListItem::new(
                    key.to_string(),
                    format!("{label}{}", home_preset_hint(*key)),
                )
            })
            .collect();
        app.presets.paint(
            items,
            Rect::new(0, body_y, area.width, preset_height),
            app.preset_focus.then_some(app.preset_selected),
            frame,
            theme,
        );
    }
    let body = Rect::new(
        0,
        body_y.saturating_add(preset_height),
        area.width,
        body_height.saturating_sub(preset_height),
    );
    render_workspaces(app, body, frame, theme);
    render_footer(app, area, frame, theme);
}

fn render_header(
    app: &ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
        &Line::from_spans(vec![Span::styled(
            format!(" Bcode Reviews  {} ", review_home_summary_label(app)),
            theme.focused.add_modifier(Modifier::BOLD),
        )]),
    );
    let filter_label = active_filter_label(app);
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, area.y.saturating_add(1), area.width, 1)),
        &Line::from_spans(vec![Span::styled(
            format!(" {filter_label}  ·  Start a review above / resume a workspace below"),
            theme.muted,
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
    let attention_count = app
        .workspace_items
        .iter()
        .filter(|item| workspace_needs_attention(item))
        .count();
    format!(
        "{visible_count} visible · {active_count} active · {attention_count} need attention · {draft_count} drafts · {published_count} published · {setup_count} setup"
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
    let attention = if app.attention_filter_active {
        "attention:on"
    } else {
        "attention:all"
    };
    let search = if app.search_query.trim().is_empty() {
        "search:off".to_string()
    } else {
        format!("search:{}", app.search_query.trim())
    };
    format!("[{archived} {drafts} {attention} {search}]")
}

fn render_workspaces(
    app: &mut ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
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
        render_workspace_list(app, list_area, frame, theme);
        render_workspace_details(app, details_area, frame, theme);
    } else {
        render_workspace_list(app, area, frame, theme);
    }
}

fn render_empty_review_home(
    app: &ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    if area.is_empty() {
        return;
    }
    let body = [Line::from(if app.workspace_items.is_empty() {
        "Choose a preset above to start reviewing immediately. Custom review opens Build, where you can combine sources. Saved reviews appear here to resume later."
    } else {
        "No workspaces match these filters. Clear search or change the draft/attention filter to see saved reviews."
    })];
    let component = EmptyStateComponent::new(
        "review-home-empty",
        if app.workspace_items.is_empty() {
            "Your next review starts here"
        } else {
            "No matching reviews"
        },
    )
    .body(&body)
    .styles(EmptyStateStyles {
        title: theme.focused,
        body: theme.muted,
        background: theme.canvas,
        ..EmptyStateStyles::default()
    });
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::terminal(Rect::new(0, 0, area.width, area.height)),
        |child| component.paint(&layout, child),
    );
}

fn render_workspace_list(
    app: &mut ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    let visible = app.visible_indices();
    if app.workspace_items.is_empty() {
        render_empty_review_home(app, area, frame, theme);
        return;
    }
    if visible.is_empty() {
        render_empty_review_home(app, area, frame, theme);
        return;
    }
    let items = visible
        .into_iter()
        .filter_map(|index| app.workspace_item(index))
        .map(|item| {
            SelectableListItem::new(
                item.workspace.id.clone(),
                format!(
                    "{}  {}",
                    workspace_health_label(item),
                    workspace_row_text(item)
                ),
            )
        })
        .collect();
    app.workspaces
        .paint(items, area, Some(app.selected), frame, theme);
}

fn render_workspace_details(
    app: &ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    let Some(index) = app.selected_workspace_index() else {
        frame.write_line(
            LocalRect::terminal(Rect::new(area.x, area.y, area.width, 1)),
            &Line::from_spans(vec![Span::styled(" No review selected", theme.muted)]),
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
        format!(" repo: {}", display_from_current_dir(&workspace.repo_root)),
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
    append_workspace_detail_progress(&mut lines, workspace, area.height);
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
            LocalRect::terminal(Rect::new(
                area.x,
                area.y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                area.width,
                1,
            )),
            &Line::from_spans(vec![Span::styled(
                line.clone(),
                if row == 0 {
                    theme.focused.add_modifier(Modifier::BOLD)
                } else {
                    theme.muted
                },
            )]),
        );
    }
}

fn append_workspace_detail_progress(
    lines: &mut Vec<String>,
    workspace: &ReviewWorkspace,
    area_height: u16,
) {
    lines.push(format!(" viewed: {} file(s)", workspace.viewed_files.len()));
    for path in workspace.viewed_files.iter().take(
        usize::from(area_height)
            .saturating_sub(lines.len())
            .saturating_sub(2),
    ) {
        lines.push(format!("   ✓ {path}"));
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
        ReviewSourceKind::Commits { .. } => "commits",
        ReviewSourceKind::PullRequest { .. } => "pull request",
        ReviewSourceKind::StackLayer { .. } => "stack layer",
        ReviewSourceKind::Stack { .. } => "stack",
        ReviewSourceKind::CurrentBranch { .. } => "current branch",
        ReviewSourceKind::Unsupported { .. } => "unsupported",
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
        || (workspace_needs_attention(item) && "needs attention".contains(query))
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
        || display_from_current_dir(&workspace.repo_root)
            .to_string()
            .to_ascii_lowercase()
            .contains(query)
        || workspace
            .archived_at_ms
            .is_some_and(|archived_at_ms| archived_at_ms.to_string().contains(query))
        || workspace
            .viewed_files
            .iter()
            .any(|path| path.to_ascii_lowercase().contains(query))
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
        other => other.label().to_ascii_lowercase(),
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
    } else if has_unpublished_drafts(item) {
        "changed since publish"
    } else if item.draft_count > 0 && item.last_publish.is_none() {
        "drafts"
    } else if item.last_publish.is_some() {
        "published"
    } else {
        "active"
    }
}

fn has_unpublished_drafts(item: &ReviewWorkspaceListItem) -> bool {
    item.draft_count > 0
        && item.last_publish.as_ref().is_some_and(|publish| {
            item.workspace
                .updated_at_ms
                .is_some_and(|updated_at_ms| updated_at_ms > publish.created_at_ms)
        })
}

fn workspace_needs_attention(item: &ReviewWorkspaceListItem) -> bool {
    matches!(
        workspace_health_label(item),
        "setup" | "needs sources" | "changed since publish" | "drafts"
    ) || item.thread_count > 0
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
    if has_unpublished_drafts(item) {
        return "open and publish draft updates".to_string();
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
    let publish_suffix = item.last_publish.as_ref().map_or_else(
        || "never published".to_string(),
        |record| {
            format!(
                "published {} via {}",
                relative_time_label(record.created_at_ms),
                record.publisher_id
            )
        },
    );
    let viewed_suffix = if workspace.viewed_files.is_empty() {
        "viewed 0".to_string()
    } else {
        format!("viewed {}", workspace.viewed_files.len())
    };
    format!(
        " {}  · {}  · {health}  · {}  · {publish_suffix}  · {viewed_suffix}  · next: {}  · {}{}",
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

fn render_help(
    app: &mut ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    let mut lines = vec![
        "Review home — choose a source or resume a saved workspace".to_owned(),
        "Presets create a review immediately. Custom opens Build to combine sources.".to_owned(),
        "Resume opens an existing review; setup workspaces open in Build.".to_owned(),
        "Click to activate. Scroll over a list to browse it.".to_owned(),
    ];
    lines.extend(crate::keybindings::hints().unwrap_or_default());
    let width = area.width.min(84);
    let popup = Rect::new(area.width.saturating_sub(width) / 2, 0, width, area.height);
    let items = lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| SelectableListItem::new(index.to_string(), line))
        .collect();
    app.help.paint(
        items,
        popup,
        Some(app.help.state.focused().unwrap_or(0)),
        frame,
        theme,
    );
}

fn home_footer_hints() -> String {
    let hints = crate::keybindings::hints().unwrap_or_default();
    [
        "Help",
        "Open review",
        "Focus next control",
        "Search",
        "New review",
        "Exit",
    ]
    .into_iter()
    .filter_map(|description| {
        hints
            .iter()
            .find(|hint| hint.ends_with(description))
            .cloned()
    })
    .collect::<Vec<_>>()
    .join("  ")
}

fn render_footer(
    app: &ReviewHomeApp,
    area: Rect,
    frame: &mut PaintCx<'_, '_>,
    theme: ReviewHomeTheme,
) {
    if area.height >= 2 {
        let context = app.status_message.clone().unwrap_or_else(|| {
            if app.preset_focus {
                HOME_PRESETS[app.preset_selected].2.to_owned()
            } else {
                "Resume a saved workspace below, or choose a source preset above.".to_owned()
            }
        });
        frame.write_line(
            LocalRect::terminal(Rect::new(0, area.height.saturating_sub(2), area.width, 1)),
            &Line::from_spans(vec![Span::styled(context, theme.muted)]),
        );
    }
    let text = app.new_review_buffer.as_ref().map_or_else(
        || {
            app.rename_buffer.as_ref().map_or_else(
                || {
                    if app.search_active {
                        format!("search: {}", app.search_query)
                    } else {
                        home_footer_hints()
                    }
                },
                |title| format!("rename: {title}"),
            )
        },
        |title| format!("new review title: {title}"),
    );
    frame.write_line(
        LocalRect::terminal(Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            1,
        )),
        &Line::from_spans(vec![Span::styled(
            text.as_str(),
            theme.canvas.patch(theme.text),
        )]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewPublishRecord;

    fn paint_home(app: &mut ReviewHomeApp, width: u16, height: u16) {
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        render(
            app,
            &mut PaintCx::new(&mut frame),
            ReviewHomeTheme::resolve(None),
        );
    }

    fn mouse(kind: bmux_tui::event::MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(bmux_tui::event::MouseEvent::new(
            kind,
            bmux_tui::geometry::Point::new(x, y),
        ))
    }

    #[test]
    fn out_of_order_archive_completions_remove_only_the_requested_ids() {
        for completion_order in [[0, 1], [1, 0]] {
            let mut app = ReviewHomeApp::new(
                PathBuf::from("/repo"),
                ["first", "second", "survivor"]
                    .into_iter()
                    .map(|id| item(workspace(id, vec![], false)))
                    .collect(),
            );
            let mut pending = None;
            assert!(queue_archive_selected_workspace(&mut app, &mut pending));
            let Some(ReviewHomeAsyncAction::ArchiveSelectedWorkspace {
                workspace_id: first_id,
            }) = pending.take()
            else {
                panic!("expected first archive request");
            };
            app.selected = 1;
            assert!(queue_archive_selected_workspace(&mut app, &mut pending));
            let Some(ReviewHomeAsyncAction::ArchiveSelectedWorkspace {
                workspace_id: second_id,
            }) = pending.take()
            else {
                panic!("expected second archive request");
            };
            let requested_ids = [first_id, second_id];
            // A reload can replace and reorder the collection while both tasks run.
            app.workspace_items.reverse();
            app.selected = 0;
            for request_index in completion_order {
                apply_home_async_result(
                    &mut app,
                    ReviewHomeAsyncResult::ArchiveSelectedWorkspace(Ok(
                        ArchiveSelectedWorkspaceResult {
                            workspace_id: requested_ids[request_index].clone(),
                            archived: true,
                        },
                    )),
                );
                assert_eq!(app.selected_workspace().unwrap().id, "survivor");
                assert!(
                    !app.workspace_items
                        .iter()
                        .any(|item| item.workspace.id == requested_ids[request_index])
                );
            }
            assert_eq!(app.workspace_items.len(), 1);
            // An absent/duplicate completion must not remove the remaining row.
            apply_home_async_result(
                &mut app,
                ReviewHomeAsyncResult::ArchiveSelectedWorkspace(Ok(
                    ArchiveSelectedWorkspaceResult {
                        workspace_id: requested_ids[0].clone(),
                        archived: true,
                    },
                )),
            );
            assert_eq!(app.workspace_items.len(), 1);
            assert_eq!(app.workspace_items[0].workspace.id, "survivor");
        }
    }

    #[test]
    fn restore_and_rename_completions_follow_ids_after_reordering() {
        let original = workspace("target", vec![], true);
        let other = workspace("other", vec![], false);
        let mut app = ReviewHomeApp::new(
            PathBuf::from("/repo"),
            vec![item(other.clone()), item(original.clone())],
        );
        let mut restored = original;
        restored.archived_at_ms = None;
        apply_home_async_result(
            &mut app,
            ReviewHomeAsyncResult::RestoreSelectedWorkspace(Ok(restored.clone())),
        );
        assert_eq!(app.workspace_items[0].workspace, other);
        assert_eq!(app.workspace_items[1].workspace, restored);
        app.workspace_items.reverse();
        app.selected = 1;
        let mut renamed = restored;
        renamed.title = "Renamed target".to_owned();
        apply_home_async_result(
            &mut app,
            ReviewHomeAsyncResult::SubmitRename(Ok(renamed.clone())),
        );
        assert_eq!(app.workspace_items[0].workspace, renamed);
        assert_eq!(app.workspace_items[1].workspace, other);
        assert_eq!(app.selected_workspace().unwrap().id, "other");
        app.workspace_items.remove(0);
        apply_home_async_result(&mut app, ReviewHomeAsyncResult::SubmitRename(Ok(renamed)));
        assert_eq!(app.workspace_items.len(), 1);
        assert_eq!(app.workspace_items[0].workspace, other);
    }

    #[test]
    fn reload_preserves_selected_workspace_identity_not_row_index() {
        let first = item(workspace("first", vec![], false));
        let second = item(workspace("second", vec![], false));
        let mut app =
            ReviewHomeApp::new(PathBuf::from("/repo"), vec![first.clone(), second.clone()]);
        app.selected = 1;
        apply_home_async_result(
            &mut app,
            ReviewHomeAsyncResult::ToggleArchived(Ok(ToggleArchivedResult {
                include_archived: true,
                workspaces: vec![second, first.clone()],
            })),
        );
        assert_eq!(app.selected_workspace().unwrap().id, "second");
        assert_eq!(app.selected, 0);
        apply_home_async_result(
            &mut app,
            ReviewHomeAsyncResult::ToggleArchived(Ok(ToggleArchivedResult {
                include_archived: false,
                workspaces: vec![first],
            })),
        );
        assert_eq!(app.selected_workspace().unwrap().id, "first");
    }

    #[test]
    fn painted_presets_activate_the_same_async_actions_as_keyboard() {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        for (index, (_, key, _)) in HOME_PRESETS.iter().enumerate() {
            let mut pointer_app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
            paint_home(&mut pointer_app, 90, 24);
            let area = pointer_app.presets.painted_area.unwrap();
            let y = area.y + u16::try_from(index).unwrap();
            assert!(
                home_pointer_event(
                    &mut pointer_app,
                    &mouse(MouseEventKind::Down(MouseButton::Left), 2, y)
                )
                .0
            );
            let (_, action) = home_pointer_event(
                &mut pointer_app,
                &mouse(MouseEventKind::Up(MouseButton::Left), 2, y),
            );
            let mut pointer_pending = None;
            assert!(handle_plugin_key_event(
                &mut pointer_app,
                action.unwrap(),
                &mut pointer_pending
            ));
            let mut keyboard_app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
            let mut keyboard_pending = None;
            assert!(handle_plugin_key_event(
                &mut keyboard_app,
                KeyStroke::simple(KeyCode::Char(*key)),
                &mut keyboard_pending
            ));
            assert!(
                matches!((pointer_pending, keyboard_pending),
            (Some(ReviewHomeAsyncAction::CreatePreset { title: pointer_title, source_kinds: pointer_sources }), Some(ReviewHomeAsyncAction::CreatePreset { title: keyboard_title, source_kinds: keyboard_sources })) if pointer_title == keyboard_title && pointer_sources == keyboard_sources)
                    || pointer_app.new_review_buffer.is_some()
            );
            assert_eq!(
                pointer_app.new_review_buffer,
                keyboard_app.new_review_buffer
            );
        }
    }

    #[test]
    fn navigation_reaches_custom_without_losing_workspace_selection() {
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
        app.selected = 7;
        assert_eq!(home_navigation(&mut app, KeyCode::Tab), None);
        for _ in 0..10 {
            let _ = home_navigation(&mut app, KeyCode::Down);
        }
        assert_eq!(
            home_navigation(&mut app, KeyCode::Enter),
            Some(KeyCode::Char('n'))
        );
        assert_eq!(app.selected, 7);
        let _ = home_navigation(&mut app, KeyCode::Tab);
        assert!(!app.preset_focus);
    }

    #[test]
    fn tiny_unicode_home_clears_old_pointer_geometry() {
        use bmux_tui::event::MouseEventKind;
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
        app.status_message = Some("审查 👩‍💻 e\u{301} ".repeat(100));
        for width in [0, 1, 2, 8, 80] {
            for height in [0, 1, 2, 4, 24] {
                paint_home(&mut app, width, height);
                for list in [&app.presets, &app.workspaces] {
                    if let Some(area) = list.painted_area {
                        assert!(area.right() <= width && area.bottom() <= height);
                    }
                }
            }
        }
        paint_home(&mut app, 0, 0);
        assert_eq!(
            home_pointer_event(&mut app, &mouse(MouseEventKind::Move, 2, 2)),
            (false, None)
        );
    }

    #[test]
    fn wheel_scroll_is_retained_and_modal_help_blocks_home_actions() {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
        paint_home(&mut app, 50, 12);
        let area = app.presets.painted_area.unwrap();
        home_pointer_event(&mut app, &mouse(MouseEventKind::ScrollDown, 2, area.y));
        let offset = app.presets.state.vertical_scroll();
        assert!(offset > 0);
        paint_home(&mut app, 50, 12);
        assert_eq!(app.presets.state.vertical_scroll(), offset);
        app.help_visible = true;
        paint_home(&mut app, 50, 12);
        assert!(app.presets.painted_area.is_none());
        assert!(
            home_pointer_event(
                &mut app,
                &mouse(MouseEventKind::Down(MouseButton::Left), 2, 2)
            )
            .1
            .is_none()
        );
        assert!(
            home_pointer_event(
                &mut app,
                &mouse(MouseEventKind::Up(MouseButton::Left), 2, 2)
            )
            .1
            .is_none()
        );
        assert_eq!(home_navigation(&mut app, KeyCode::Char('u')), None);
        assert_eq!(home_navigation(&mut app, KeyCode::Escape), None);
        assert!(!app.help_visible);
    }

    #[test]
    fn painted_workspace_click_resumes_selected_unicode_workspace() {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let mut saved = workspace("审查 👩‍💻 e\u{301}", vec![], false);
        saved.title = "非常长的工作区 👩‍💻 e\u{301} ".repeat(30);
        let item = ReviewWorkspaceListItem {
            workspace: saved.clone(),
            thread_count: 0,
            draft_count: 0,
            last_publish: None,
        };
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![item]);
        paint_home(&mut app, 80, 24);
        let area = app.workspaces.painted_area.unwrap();
        home_pointer_event(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), area.x + 1, area.y),
        );
        let (_, action) = home_pointer_event(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), area.x + 1, area.y),
        );
        assert_eq!(action, Some(KeyStroke::simple(KeyCode::Enter)));
        let mut pending = None;
        handle_plugin_key_event(&mut app, action.unwrap(), &mut pending);
        assert_eq!(
            app.outcome,
            Some(ReviewHomeOutcome::OpenWorkspace {
                workspace: saved,
                build_mode: true
            })
        );
    }

    #[test]
    fn effective_preset_hints_do_not_reintroduce_default_shortcuts() {
        // Effective hint matching uses the declared description, not the default shortcut.
        let definition = crate::keybindings::actions_for(crate::keybindings::HOME)
            .into_iter()
            .find(|action| action.defaults.iter().any(|key| key == "u"))
            .unwrap();
        crate::keybindings::set_hints(vec![format!("alt+u {}", definition.description)]);
        assert_eq!(home_preset_hint('u'), "  [alt+u]");
        crate::keybindings::set_hints(vec![]);
        assert_eq!(home_preset_hint('u'), "");
        crate::keybindings::clear_hints();
    }

    #[test]
    fn releasing_outside_a_preset_cancels_activation() {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
        paint_home(&mut app, 80, 24);
        let y = app.presets.painted_area.unwrap().y;
        home_pointer_event(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), 2, y),
        );
        assert!(
            home_pointer_event(
                &mut app,
                &mouse(MouseEventKind::Up(MouseButton::Left), 2, 23)
            )
            .1
            .is_none()
        );
        assert!(
            home_pointer_event(
                &mut app,
                &mouse(MouseEventKind::Up(MouseButton::Left), 2, y)
            )
            .1
            .is_none()
        );
    }

    #[test]
    fn resize_invalidates_all_previous_home_hits() {
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), vec![]);
        paint_home(&mut app, 100, 30);
        assert!(app.presets.painted_area.is_some());
        assert!(
            home_pointer_event(
                &mut app,
                &Event::Resize(bmux_tui::geometry::Size::new(1, 1))
            )
            .0
        );
        assert!(app.presets.painted_area.is_none());
        assert!(app.workspaces.painted_area.is_none());
    }

    fn workspace(id: &str, sources: Vec<ReviewSource>, archived: bool) -> ReviewWorkspace {
        ReviewWorkspace {
            id: id.to_string(),
            title: id.to_string(),
            repo_root: PathBuf::from("/repo"),
            sources,
            created_at_ms: Some(1),
            updated_at_ms: Some(2),
            viewed_files: std::collections::BTreeSet::new(),
            archived_at_ms: archived.then_some(3),
            presentation_state: None,
            viewed_marks: Vec::new(),
            reviewed_revisions: std::collections::BTreeMap::new(),
            presentation_v2: None,
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
    fn escape_never_exits_review_home() {
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), Vec::new());
        let mut pending_action = None;

        assert!(!handle_plugin_key_event(
            &mut app,
            KeyStroke::simple(KeyCode::Escape),
            &mut pending_action,
        ));

        assert!(!app.should_exit);
        assert_eq!(app.outcome, None);
    }

    #[test]
    fn control_d_exits_review_home() {
        let mut app = ReviewHomeApp::new(PathBuf::from("/repo"), Vec::new());
        let mut pending_action = None;
        let stroke = KeyStroke {
            key: KeyCode::Char('d'),
            modifiers: bmux_keyboard::Modifiers {
                ctrl: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        };

        assert!(handle_plugin_key_event(
            &mut app,
            stroke,
            &mut pending_action,
        ));

        assert!(app.should_exit);
        assert_eq!(app.outcome, Some(ReviewHomeOutcome::Exit));
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
            Some("review is archived; restore it before opening")
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
            published_thread_anchors: Vec::new(),
            output: Some("https://example.test/pr/1".to_string()),
            message: "submitted".to_string(),
            created_at_ms: 4,
        });
        let setup = item(workspace("new-review", Vec::new(), false));

        assert!(workspace_item_matches_query(&setup, "setup"));
        assert!(workspace_item_matches_query(&setup, "needs attention"));
        assert!(workspace_item_matches_query(&published, "published"));
        assert!(workspace_item_matches_query(&published, "github"));
        assert!(workspace_item_matches_query(&published, "example.test"));
        published
            .workspace
            .viewed_files
            .insert("src/lib.rs".to_string());
        assert!(workspace_item_matches_query(&published, "src/lib"));
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
            published_thread_anchors: Vec::new(),
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
        published.workspace.updated_at_ms = Some(5);
        published.draft_count = 1;
        assert_eq!(workspace_health_label(&published), "changed since publish");
        assert_eq!(
            workspace_next_action(&published),
            "open and publish draft updates"
        );
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
        assert!(app.select_next_attention_review());
        assert_eq!(app.selected, 0);
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

    #[test]
    fn attention_filter_shows_reviews_needing_work() {
        let mut draft = item(workspace("draft", vec![source("source", true)], false));
        draft.draft_count = 1;
        let app = ReviewHomeApp {
            attention_filter_active: true,
            workspace_items: vec![
                item(workspace("setup", Vec::new(), false)),
                item(workspace("active", vec![source("source", true)], false)),
                draft,
            ],
            ..ReviewHomeApp::new(PathBuf::from("/repo"), Vec::new())
        };

        assert_eq!(app.visible_indices(), vec![0, 2]);
    }

    #[test]
    fn summary_counts_reviews_needing_attention() {
        let mut draft = item(workspace("draft", vec![source("source", true)], false));
        draft.draft_count = 1;
        let app = ReviewHomeApp::new(
            PathBuf::from("/repo"),
            vec![
                item(workspace("setup", Vec::new(), false)),
                item(workspace("active", vec![source("source", true)], false)),
                draft,
            ],
        );

        assert!(review_home_summary_label(&app).contains("2 need attention"));
    }
}
