//! Worktree terminal presentation and local actions; never model tool routing.

use super::{
    PathBuf, WORKTREE_CREATE_SCHEMA, WORKTREE_LIST_SCHEMA, WORKTREE_REMOVE_SCHEMA,
    WORKTREE_REQUEST_SCHEMA, WorktreeCreateRequest, WorktreeInfo, display,
};
use bcode_tui_components::tool_card::{ToolCardStyle, push_tool_card_detail, tool_card_header};
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::event::Event;
#[cfg(test)]
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBarComponent, KeyHintBarStyles};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListOutcome, SelectableListState,
    SelectableListStyles,
};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles,
};
use std::str::FromStr;

#[must_use]
pub fn worktree_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    for (scope, actions) in [
        ("worktree.list", ControlAction::NAVIGATION),
        ("worktree.create", ControlAction::TEXT),
    ] {
        for action in actions {
            let mut definition = action.definition(scope);
            if scope == "worktree.list" {
                match *action {
                    ControlAction::Up => definition.defaults.push("k".into()),
                    ControlAction::Down => definition.defaults.push("j".into()),
                    _ => {}
                }
            }
            registry.register_keymap_action(definition);
        }
        if scope == "worktree.list" {
            registry.register_keymap_action(ControlAction::Toggle.definition(scope));
        }
        for action in [ControlAction::Submit, ControlAction::Cancel] {
            let mut definition = action.definition(scope);
            if scope == "worktree.list" && action == ControlAction::Cancel {
                definition.defaults.push("q".into());
            }
            registry.register_keymap_action(definition);
        }
    }
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.attach",
        title: "Attach Worktree",
    }));
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.createSession",
        title: "Create Worktree Session",
    }));
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.remove",
        title: "Remove Worktree",
    }));
    registry.register_visual_adapter(
        [
            "worktree-request-card",
            "worktree-list-card",
            "worktree-create-result-card",
            "worktree-remove-result-card",
        ],
        Box::new(WorktreeTuiVisualAdapter),
    );
    registry
}

struct WorktreeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for WorktreeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            WORKTREE_REQUEST_SCHEMA
                | WORKTREE_LIST_SCHEMA
                | WORKTREE_CREATE_SCHEMA
                | WORKTREE_REMOVE_SCHEMA
        )
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
    }

    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let style = worktree_tool_card_style(context);
        match kind {
            WORKTREE_REQUEST_SCHEMA => worktree_request_rows(payload, context, style),
            WORKTREE_LIST_SCHEMA => worktree_list_rows(payload, context, style),
            WORKTREE_CREATE_SCHEMA => {
                worktree_result_rows("Worktree created", payload, context, style)
            }
            WORKTREE_REMOVE_SCHEMA => {
                worktree_result_rows("Worktree removed", payload, context, style)
            }
            _ => Vec::new(),
        }
    }
}

fn worktree_request_rows(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = worktree_header("Worktree request", style);
    for key in [
        "operation",
        "cwd",
        "name",
        "path",
        "branch",
        "new_branch",
        "base_ref",
        "detach",
        "force",
        "no_setup",
    ] {
        push_visual_kv(
            &mut rows,
            key,
            if matches!(key, "cwd" | "path") {
                visual_text(arguments, key).map(|path| context.display_path(path).to_string())
            } else {
                visual_value(arguments, key)
            },
            style,
        );
    }
    rows
}

fn worktree_list_rows(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let values = payload
        .get("worktrees")
        .or_else(|| payload.get("entries"))
        .and_then(serde_json::Value::as_array);
    let count = values.map_or(0, Vec::len);
    let mut rows = worktree_header(&format!("Worktrees ({count})"), style);
    if let Some(values) = values {
        for value in values.iter().take(20) {
            let path = visual_text(value, "path").unwrap_or("<path>");
            let branch = visual_text(value, "branch").or_else(|| visual_text(value, "name"));
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", style.accent),
                Span::styled(context.display_path(path).to_string(), style.value),
                Span::styled(
                    branch.map_or_else(String::new, |branch| format!("  {branch}")),
                    style.muted,
                ),
            ]));
        }
        if values.len() > 20 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more worktrees", values.len() - 20),
                style.muted,
            )]));
        }
    }
    rows
}

fn worktree_result_rows(
    title: &str,
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let mut rows = worktree_header(title, style);
    for key in ["path", "branch", "name", "session_id", "removed", "force"] {
        push_visual_kv(
            &mut rows,
            key,
            if key == "path" {
                visual_text(payload, key).map(|path| context.display_path(path).to_string())
            } else {
                visual_value(payload, key)
            },
            style,
        );
    }
    rows
}

fn worktree_header(title: &str, style: ToolCardStyle) -> Vec<Line> {
    vec![tool_card_header(
        Span::styled("◆ ", style.accent),
        Span::styled(title.to_string(), style.title),
    )]
}

fn push_visual_kv<T>(rows: &mut Vec<Line>, key: &str, value: Option<T>, style: ToolCardStyle)
where
    T: Into<String>,
{
    if let Some(value) = value.map(Into::into) {
        push_tool_card_detail(rows, key, Some(&value), style.muted, style.value);
    }
}

fn visual_value(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                value
                    .as_bool()
                    .map(|value| if value { "yes" } else { "no" }.to_string())
            })
            .or_else(|| value.as_u64().map(|value| value.to_string()))
    })
}

fn visual_text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn worktree_tool_card_style(
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> ToolCardStyle {
    ToolCardStyle::from_component_theme(context.theme().and_then(|theme| theme.component_theme()))
}

struct WorktreeCommandSurfaceFactory {
    surface_kind: &'static str,
    title: &'static str,
}

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for WorktreeCommandSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        self.surface_kind
    }

    fn open(
        &self,
        request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
    ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
        let surface_kind = self.surface_kind;
        let title = self.title;
        Box::pin(async move {
            let Some(repo_path) = request.repo_path else {
                return Err("worktree surfaces require an explicit repo path".into());
            };
            let (lines, worktrees) = worktree_surface_state(surface_kind, &repo_path);
            let session_id = request
                .options
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| bcode_session_models::SessionId::from_str(value).ok());
            Ok(Box::new(WorktreeCommandSurface {
                hints: Vec::new(),
                id: surface_kind,
                title,
                repo_path,
                lines,
                worktrees,
                selected: 0,
                list_area: Rect::new(0, 0, 0, 0),
                status: None,
                create_name: "new-session".to_string(),
                create_input: TextInputState::new(TextEditBuffer::from_text("new-session")),
                input_area: Rect::new(0, 0, 0, 0),
                session_id,
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct WorktreeCommandSurface {
    hints: Vec<(String, String)>,
    id: &'static str,
    title: &'static str,
    repo_path: PathBuf,
    lines: Vec<String>,
    worktrees: Vec<WorktreeInfo>,
    selected: usize,
    list_area: Rect,
    status: Option<String>,
    create_name: String,
    create_input: TextInputState,
    input_area: Rect,
    session_id: Option<bcode_session_models::SessionId>,
}

struct WorktreeSurfaceTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    selection: Style,
}

impl WorktreeSurfaceTheme {
    fn resolve(theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
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

impl WorktreeCommandSurface {
    fn render_themed(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = WorktreeSurfaceTheme::resolve(theme);
        frame.fill(LocalRect::terminal(area), " ", theme.canvas);
        write_line(
            frame,
            area,
            area.y,
            Line::from_spans(vec![Span::styled(
                self.title,
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        write_line(
            frame,
            area,
            area.y.saturating_add(1),
            Line::from_spans(vec![Span::styled(
                format!("Repo: {}", display(&self.repo_path, &self.repo_path)),
                theme.muted,
            )]),
        );
        let mut y = area.y.saturating_add(3);
        if self.is_selectable() {
            write_line(
                frame,
                area,
                y,
                Line::from_spans(vec![Span::styled(&self.lines[0], theme.text)]),
            );
            y = y.saturating_add(1);
            let items = self
                .worktrees
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    SelectableListItem::new(index.to_string(), self.lines[index + 1].clone())
                })
                .collect::<Vec<_>>();
            let list = SelectableList::new(&items).styles(worktree_list_styles(&theme));
            let list_height = u16::try_from(items.len()).unwrap_or(u16::MAX);
            self.list_area = Rect::new(
                area.x,
                y,
                area.width,
                list_height.min(area.bottom().saturating_sub(y).saturating_sub(2)),
            );
            let state = worktree_list_state(self.selected);
            list.paint(self.list_area, &state, theme.canvas, frame);
        } else {
            self.list_area = Rect::new(0, 0, 0, 0);
            for line in &self.lines {
                write_line(
                    frame,
                    area,
                    y,
                    Line::from_spans(vec![Span::styled(line, theme.text)]),
                );
                y = y.saturating_add(1);
            }
        }
        if self.id == "command.work-tree.createSession" {
            let retained = std::cell::RefCell::new(self.create_input.clone());
            let input =
                TextInputBoxComponent::new("worktree.name", single_line_text_policy(), &retained)
                    .label("Name")
                    .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)))
                    .styles(worktree_input_styles(&theme));
            self.input_area = Rect::new(
                area.x,
                y,
                area.width,
                4.min(area.bottom().saturating_sub(y).saturating_sub(2)),
            );
            let area = self.input_area;
            let layout = input.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            frame.with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| input.paint(&layout, cx),
            );
            self.create_input = retained.into_inner();
            let local = self.create_input.content_area();
            self.create_input.set_content_area(
                Rect::new(
                    area.x.saturating_add(local.x),
                    area.y.saturating_add(local.y),
                    local.width,
                    local.height,
                ),
                &single_line_text_policy(),
            );
            self.create_name = self.create_input.buffer().text().to_owned();
        } else {
            self.input_area = Rect::new(0, 0, 0, 0);
        }
        self.render_footer(area, frame, &theme);
    }

    fn render_footer(&self, area: Rect, frame: &mut PaintCx<'_, '_>, theme: &WorktreeSurfaceTheme) {
        if let Some(status) = &self.status {
            write_line(
                frame,
                area,
                area.y.saturating_add(area.height.saturating_sub(2)),
                Line::from_spans(vec![Span::styled(status.clone(), theme.focused)]),
            );
        }
        let hints = self
            .hints
            .iter()
            .filter(|(keys, _)| !keys.is_empty())
            .map(|(keys, label)| KeyHint::new(keys.as_str(), label.as_str()))
            .collect::<Vec<_>>();
        let hints =
            KeyHintBarComponent::new("worktree.hints", &hints).styles(worktree_hint_styles(theme));
        let layout = hints.layout(
            Constraints::tight(bmux_tui::geometry::Size::new(area.width, 1)),
            &mut LayoutCx::new(),
        );
        frame.with_child(
            i32::from(area.x),
            i64::from(area.bottom().saturating_sub(1)),
            LocalRect::new(0, 0, area.width, 1),
            |cx| hints.paint(&layout, cx),
        );
    }
}

fn worktree_list_styles(theme: &WorktreeSurfaceTheme) -> SelectableListStyles {
    SelectableListStyles {
        background: theme.canvas,
        scrollbar: bmux_tui_components::scrollbar::ScrollbarStyles::default(),
        normal: theme.text,
        focused: theme.selection,
        selected: theme.selection,
        hovered: theme.focused,
        pressed: theme.selection.add_modifier(Modifier::BOLD),
        disabled: theme.muted,
    }
}

const fn worktree_list_state(selected: usize) -> SelectableListState {
    let mut state = SelectableListState::new(None);
    state.set_focused(Some(selected));
    state
}

const fn single_line_text_policy() -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    policy.keyboard.shift_enter = None;
    policy.viewport.max_rows = Some(1);
    policy
}

const fn worktree_input_styles(theme: &WorktreeSurfaceTheme) -> TextInputBoxStyles {
    TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.focused,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.selection,
        border: theme.muted,
        focused_border: theme.focused,
        background: theme.canvas,
        focused_background: theme.canvas,
        disabled_background: theme.canvas,
    }
}

const fn worktree_hint_styles(theme: &WorktreeSurfaceTheme) -> KeyHintBarStyles {
    KeyHintBarStyles {
        key: theme.focused,
        label: theme.text,
        separator: theme.muted,
        disabled: theme.muted,
        background: theme.canvas,
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for WorktreeCommandSurface {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> &'static str {
        self.title
    }

    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.render_themed(area, frame, None);
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        self.render_themed(area, frame, theme);
    }

    fn render_with_host(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) {
        use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
        let scope = if self.id == "command.work-tree.createSession" {
            "worktree.create"
        } else {
            "worktree.list"
        };
        self.hints = [
            ControlAction::Up,
            ControlAction::Down,
            ControlAction::Submit,
            ControlAction::Cancel,
        ]
        .map(|action| (host.keymap_keys(scope, &action.id()).join("/"), action.id()))
        .into();
        self.render_themed(area, frame, theme);
    }

    fn handle_event(
        &mut self,
        event: &Event,
        host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        use bcode_plugin_sdk::tui::keymap::controls::{ControlAction, ControlInput};
        let creating = self.id == "command.work-tree.createSession";
        let scope = if creating {
            "worktree.create"
        } else {
            "worktree.list"
        };
        let supported = if creating {
            ControlAction::TEXT
        } else {
            ControlAction::NAVIGATION
        };
        let event = match event {
            Event::Key(stroke) => {
                if host.keymap_action(scope, *stroke).as_deref()
                    == Some(ControlAction::Cancel.id().as_str())
                {
                    return bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None };
                }
                if host.keymap_action(scope, *stroke).as_deref()
                    == Some(ControlAction::Submit.id().as_str())
                    || !creating
                        && host.keymap_action(scope, *stroke).as_deref()
                            == Some(ControlAction::Toggle.id().as_str())
                {
                    if creating {
                        self.create_name = self.create_input.buffer().text().to_owned();
                        return self.create_worktree();
                    }
                    return self.activate_selected();
                }
                let Some(stroke) =
                    ControlInput::from_host(host, scope, *stroke, supported, creating)
                        .native_stroke()
                else {
                    return bcode_plugin_sdk::tui::PluginTuiAction::None;
                };
                Event::Key(stroke)
            }
            event => event.clone(),
        };
        let event = &event;
        if self.id == "command.work-tree.createSession" {
            let policy = single_line_text_policy();
            let input = bmux_tui_components::text_input::TextInputControl::new(&policy);
            return match input.handle_event(&mut self.create_input, event) {
                bmux_tui_components::text_input::TextInputOutcome::Submitted => {
                    self.create_name = self.create_input.buffer().text().to_owned();
                    self.create_worktree()
                }
                bmux_tui_components::text_input::TextInputOutcome::Edited
                | bmux_tui_components::text_input::TextInputOutcome::Redraw => {
                    self.create_name = self.create_input.buffer().text().to_owned();
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                bmux_tui_components::text_input::TextInputOutcome::Ignored
                | bmux_tui_components::text_input::TextInputOutcome::EdgeUp
                | bmux_tui_components::text_input::TextInputOutcome::EdgeDown => {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            };
        }
        if self.is_selectable() {
            let items = self
                .worktrees
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    SelectableListItem::new(index.to_string(), self.lines[index + 1].clone())
                })
                .collect::<Vec<_>>();
            let list = SelectableList::new(&items);
            let mut state = worktree_list_state(self.selected);
            return match list.handle_event(self.list_area, &mut state, event) {
                SelectableListOutcome::Focused(index) => {
                    self.selected = index;
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                SelectableListOutcome::Redraw => {
                    self.selected = state.focused().unwrap_or(self.selected);
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                SelectableListOutcome::Selected(index) => {
                    self.selected = index;
                    self.activate_selected()
                }
                SelectableListOutcome::Ignored => bcode_plugin_sdk::tui::PluginTuiAction::None,
            };
        }
        bcode_plugin_sdk::tui::PluginTuiAction::None
    }
}

impl WorktreeCommandSurface {
    fn create_worktree(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let name = self.create_name.trim().to_string();
        if name.is_empty() {
            self.status = Some("worktree name is required".to_string());
            return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
        }
        let request = WorktreeCreateRequest {
            name,
            cwd: Some(self.repo_path.clone()),
            path: None,
            branch: None,
            new_branch: None,
            base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
            detach: false,
            force: false,
            attach_session_id: self.session_id,
            new_session: self.session_id.is_none(),
            no_setup: false,
        };
        let config_paths = bcode_config::default_config_paths_from(&self.repo_path);
        let config = match bcode_config::load_config_from_paths(&config_paths) {
            Ok(config) => config,
            Err(error) => {
                self.status = Some(format!("worktree config unavailable: {error}"));
                return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
            }
        };
        match bcode_worktree::create_worktree(&config, &request, &self.repo_path) {
            Ok(response) => bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "status": format!("created worktree {}", display(&response.path, &self.repo_path)),
                    "append_text": format!("Created worktree: {}", display(&response.path, &self.repo_path)),
                    "set_session_working_directory": response.path.display().to_string(),
                })),
            },
            Err(error) => {
                self.status = Some(format!("worktree create failed: {error}"));
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
        }
    }

    fn is_selectable(&self) -> bool {
        matches!(
            self.id,
            "command.work-tree.attach" | "command.work-tree.remove"
        ) && !self.worktrees.is_empty()
    }

    fn activate_selected(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        match self.id {
            "command.work-tree.remove" => self.remove_selected(),
            "command.work-tree.attach" => self.attach_selected(),
            _ => bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None },
        }
    }

    fn attach_selected(&self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let Some(worktree) = self.worktrees.get(self.selected) else {
            return bcode_plugin_sdk::tui::PluginTuiAction::None;
        };
        bcode_plugin_sdk::tui::PluginTuiAction::Close {
            outcome: Some(serde_json::json!({
                "status": format!("attaching worktree {}", display(&worktree.path, &self.repo_path)),
                "append_text": format!("Attaching session to worktree: {}", display(&worktree.path, &self.repo_path)),
                "set_session_working_directory": worktree.path.display().to_string(),
            })),
        }
    }

    fn remove_selected(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let Some(worktree) = self.worktrees.get(self.selected) else {
            return bcode_plugin_sdk::tui::PluginTuiAction::None;
        };
        if worktree.is_main {
            self.status = Some("refusing to remove main worktree".to_string());
            return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
        }
        match bcode_worktree::remove_worktree(&self.repo_path, &worktree.path, false) {
            Ok(response) => bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "status": format!("removed worktree {}", display(&response.path, &self.repo_path)),
                    "append_text": format!("Removed worktree: {}", display(&response.path, &self.repo_path)),
                })),
            },
            Err(error) => {
                self.status = Some(format!("worktree remove failed: {error}"));
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
        }
    }
}

fn worktree_surface_state(
    surface_kind: &str,
    repo_path: &std::path::Path,
) -> (Vec<String>, Vec<WorktreeInfo>) {
    match bcode_worktree::list_worktrees(repo_path) {
        Ok(response) => {
            let worktrees = response.worktrees;
            let mut lines = match surface_kind {
                "command.work-tree.attach" => vec!["Select a worktree to attach:".to_string()],
                "command.work-tree.remove" => vec!["Select a worktree to remove:".to_string()],
                "command.work-tree.createSession" => vec![
                    "Enter worktree name, then press Enter to create.".to_string(),
                    "Backspace edits · Esc/q cancels".to_string(),
                ],
                _ => vec!["Worktree command surface".to_string()],
            };
            lines.extend(worktrees.iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.as_deref().unwrap_or("<detached>");
                format!(
                    "* {marker} {branch} — {}",
                    display(&worktree.path, repo_path)
                )
            }));
            (lines, worktrees)
        }
        Err(error) => (vec![format!("worktrees unavailable: {error}")], Vec::new()),
    }
}

fn write_line(frame: &mut PaintCx<'_, '_>, area: Rect, y: u16, line: impl Into<Line>) {
    if y >= area.y.saturating_add(area.height) {
        return;
    }
    frame.write_line(
        LocalRect::terminal(Rect::new(area.x, y, area.width, 1)),
        &line.into(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree_request_payload;
    use bcode_plugin_sdk::tui::PluginTuiSurface;

    fn test_plugin_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffTheme, PluginTuiSourceTheme, PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
            PluginTuiTheme,
        };
        let style = Style::new();
        let syntax = PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default);
        PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style.bg(bmux_tui::style::Color::Blue),
            text: style.fg(bmux_tui::style::Color::White),
            muted: style.fg(bmux_tui::style::Color::Yellow),
            border: style.fg(bmux_tui::style::Color::Cyan),
            focused: style.fg(bmux_tui::style::Color::Magenta),
            selection: style
                .fg(bmux_tui::style::Color::Black)
                .bg(bmux_tui::style::Color::Green),
            source: PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added: style,
                removed: style,
                hunk: style,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax: PluginTuiSyntaxTheme {
                text: syntax,
                comment: syntax,
                keyword: syntax,
                function: syntax,
                variable: syntax,
                string: syntax,
                number: syntax,
                type_name: syntax,
                operator: syntax,
                punctuation: syntax,
                heading: syntax,
                link: syntax,
                raw: syntax,
            },
            component: bcode_plugin_sdk::tui::ComponentTheme::terminal_default(),
        }
        .with_legacy_component()
    }

    struct KeymapHost(bcode_plugin_sdk::tui::keymap::KeymapRegistry);
    impl bcode_plugin_sdk::tui::PluginTuiHost for KeymapHost {
        fn spawn(&self, _task: bcode_plugin_sdk::tui::PluginTask) {}
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn keymap_action(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> Option<String> {
            self.0.action_for_key(scope, stroke).map(str::to_owned)
        }
        fn keymap_is_overridden(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> bool {
            self.0.is_overridden_key(scope, stroke)
        }
    }

    #[test]
    fn worktree_navigation_remap_preserves_home_page_and_space_activation() {
        use bcode_plugin_sdk::tui::keymap::controls::{
            ControlAction, ControlInput, resolve_registry,
        };
        let definitions = worktree_tui_registry();
        let registry = resolve_registry(
            definitions.keymap_actions().iter().cloned(),
            std::collections::BTreeMap::from([(
                "worktree.list".into(),
                std::collections::BTreeMap::from([
                    ("ctrl+p".into(), ControlAction::Up.id()),
                    ("down".into(), String::new()),
                ]),
            )]),
        )
        .unwrap();
        let host = KeymapHost(registry);
        let input = |key| {
            ControlInput::from_host(
                &host as &dyn bcode_plugin_sdk::tui::PluginTuiHost,
                "worktree.list",
                bmux_keyboard::parse_key_stroke(key).unwrap(),
                ControlAction::NAVIGATION,
                false,
            )
        };
        assert_eq!(input("up"), ControlInput::Ignored);
        assert_eq!(input("k"), ControlInput::Ignored);
        assert_eq!(input("down"), ControlInput::Ignored);
        assert_eq!(input("ctrl+p"), ControlInput::Action(ControlAction::Up));
        assert_eq!(input("home"), ControlInput::Action(ControlAction::Home));
        assert_eq!(
            input("page_down"),
            ControlInput::Action(ControlAction::PageDown)
        );
        assert_eq!(
            host.0.action_for_key(
                "worktree.list",
                bmux_keyboard::parse_key_stroke("space").unwrap()
            ),
            Some(ControlAction::Toggle.id().as_str())
        );
    }

    #[test]
    fn worktree_surface_uses_host_theme_at_narrow_sizes() {
        let worktree = WorktreeInfo {
            path: PathBuf::from("/tmp/repo"),
            is_main: true,
            branch: Some("main".to_owned()),
            commit: Some("abc123".to_owned()),
        };
        let mut surface = WorktreeCommandSurface {
            hints: Vec::new(),
            id: "command.work-tree.attach",
            title: "Attach worktree",
            repo_path: PathBuf::from("/tmp/repo"),
            lines: vec!["Choose a worktree".to_owned(), "main".to_owned()],
            worktrees: vec![worktree],
            selected: 0,
            list_area: Rect::new(0, 0, 0, 0),
            status: None,
            create_name: String::new(),
            create_input: TextInputState::new(TextEditBuffer::from_text("")),
            input_area: Rect::new(0, 0, 0, 0),
            session_id: None,
        };
        let area = Rect::new(0, 0, 18, 8);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let theme = test_plugin_theme();
        surface.render_with_theme(
            area,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            Some(&theme),
        );

        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.canvas.bg)
        );
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.selection.bg)
        );
        assert!(surface.list_area.width <= area.width);
        assert!(surface.list_area.height <= area.height);
    }

    #[test]
    fn worktree_selectable_list_supports_mouse_activation() {
        let items = [
            SelectableListItem::new("first", "first"),
            SelectableListItem::new("second", "second"),
        ];
        let list = SelectableList::new(&items);
        let mut state = worktree_list_state(0);
        let area = Rect::new(4, 3, 20, 2);
        let point = bmux_tui::geometry::Point::new(6, 4);

        assert_eq!(
            list.handle_event(
                area,
                &mut state,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left,),
                    point,
                )),
            ),
            SelectableListOutcome::Redraw
        );
        assert_eq!(
            list.handle_event(
                area,
                &mut state,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                    point,
                )),
            ),
            SelectableListOutcome::Selected(1)
        );
    }

    #[test]
    fn worktree_name_input_handles_unicode_editing() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("tree"));
        let policy = single_line_text_policy();
        let input = bmux_tui_components::text_input::TextInputControl::new(&policy);

        assert_eq!(
            input.handle_event(&mut state, &Event::Paste("-🙂".to_owned())),
            bmux_tui_components::text_input::TextInputOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "tree-🙂");
    }

    #[test]
    fn worktree_requests_use_durable_generic_contributions_without_legacy_visuals() {
        let arguments = serde_json::json!({"name": "feature", "base_ref": "head"});
        let payload = worktree_request_payload("worktree.create", &arguments);
        assert_eq!(payload["operation"], "worktree.create");
        assert_eq!(payload["name"], "feature");
    }

    #[test]
    fn worktree_request_adapter_renders_generic_contribution_payload() {
        let arguments = serde_json::json!({"name": "feature", "base_ref": "head"});
        let payload = worktree_request_payload("worktree.create", &arguments);
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &WorktreeTuiVisualAdapter,
            WORKTREE_REQUEST_SCHEMA,
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            ),
        );
        let rendered = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("worktree.create"));
        assert!(rendered.contains("feature"));
        assert!(rendered.contains("head"));
    }
}
