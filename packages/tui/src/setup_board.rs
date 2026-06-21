//! Data-driven setup board-game component for onboarding.

use std::collections::{BTreeMap, BTreeSet};

use bcode_settings::{SetupSectionId, SetupSectionStatus};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::scroll_area::{ScrollArea, ScrollAreaOutcome, ScrollAreaState};

/// Semantic board spot rendered as a clickable board-game location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardSpot {
    /// Stable spot identifier.
    pub id: SetupSectionId,
    /// Primary label.
    pub title: String,
    /// Secondary location label.
    pub subtitle: String,
    /// Current setup status.
    pub status: SetupSectionStatus,
    /// Optional layout hints used by the deterministic auto-layout engine.
    pub hint: BoardSpotLayoutHint,
}

impl BoardSpot {
    /// Create a board spot.
    #[must_use]
    pub fn new(
        id: SetupSectionId,
        title: impl Into<String>,
        subtitle: impl Into<String>,
        status: SetupSectionStatus,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            subtitle: subtitle.into(),
            status,
            hint: BoardSpotLayoutHint::default(),
        }
    }

    /// Return this spot with a preferred layer hint.
    #[must_use]
    pub const fn layer(mut self, layer: u16) -> Self {
        self.hint.preferred_layer = Some(layer);
        self
    }
}

/// Optional layout hints for board spots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoardSpotLayoutHint {
    /// Preferred vertical layer. Spots in the same layer are auto-spaced.
    pub preferred_layer: Option<u16>,
}

/// Semantic connection between board spots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardConnection {
    /// Source spot.
    pub from: SetupSectionId,
    /// Destination spot.
    pub to: SetupSectionId,
}

impl BoardConnection {
    /// Create a board connection.
    #[must_use]
    pub const fn new(from: SetupSectionId, to: SetupSectionId) -> Self {
        Self { from, to }
    }
}

/// Setup board layout/rendering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupBoardPolicy {
    /// Width of every spot card.
    pub spot_width: u16,
    /// Height of every spot card.
    pub spot_height: u16,
    /// Horizontal spacing between spots in a layer.
    pub horizontal_gap: u16,
    /// Vertical spacing between layers.
    pub vertical_gap: u16,
    /// Padding around the virtual board.
    pub padding: u16,
}

impl SetupBoardPolicy {
    /// Create the default setup-board policy.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            spot_width: 18,
            spot_height: 4,
            horizontal_gap: 6,
            vertical_gap: 3,
            padding: 2,
        }
    }
}

impl Default for SetupBoardPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Runtime board state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupBoardState {
    /// Pannable virtual-board scroll state.
    pub scroll: ScrollAreaState,
    /// Currently focused spot.
    pub focused: SetupSectionId,
    /// Hovered spot, if any.
    pub hovered: Option<SetupSectionId>,
    /// Pressed spot, if any.
    pub pressed: Option<SetupSectionId>,
}

impl SetupBoardState {
    /// Create board state focused on `focused`.
    #[must_use]
    pub const fn new(focused: SetupSectionId) -> Self {
        Self {
            scroll: ScrollAreaState::new(),
            focused,
            hovered: None,
            pressed: None,
        }
    }
}

/// Outcome from board event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupBoardOutcome {
    /// Event was ignored.
    Ignored,
    /// Event was handled and should redraw.
    Redraw,
    /// Board viewport was panned/scrolled.
    Panned,
    /// A spot became focused.
    Focused(SetupSectionId),
    /// A spot was selected/clicked.
    Selected(SetupSectionId),
}

impl SetupBoardOutcome {
    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

/// Data-driven setup board component.
#[derive(Debug, Clone, Copy)]
pub struct SetupBoard<'a> {
    spots: &'a [BoardSpot],
    connections: &'a [BoardConnection],
    policy: SetupBoardPolicy,
}

impl<'a> SetupBoard<'a> {
    /// Create a setup board for spots and connections.
    #[must_use]
    pub const fn new(spots: &'a [BoardSpot], connections: &'a [BoardConnection]) -> Self {
        Self {
            spots,
            connections,
            policy: SetupBoardPolicy::new(),
        }
    }

    /// Render this board in `area`.
    pub fn render(self, area: Rect, state: &SetupBoardState, frame: &mut Frame<'_>) {
        let lines = self.render_lines(state);
        ScrollArea::new(&lines).render(area, &state.scroll, frame);
    }

    /// Handle input for this board.
    pub fn handle_event(
        self,
        area: Rect,
        state: &mut SetupBoardState,
        event: &Event,
    ) -> SetupBoardOutcome {
        let lines = self.render_lines(state);
        let scroll_area = ScrollArea::new(&lines);
        let scroll_outcome = match event {
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                ) =>
            {
                scroll_area.handle_event(area, &mut state.scroll, event)
            }
            _ => ScrollAreaOutcome::Ignored,
        };
        if let Event::Mouse(mouse) = event
            && let Some(outcome) = self.handle_mouse(area, state, *mouse)
        {
            return outcome;
        }
        let scroll_outcome = if matches!(scroll_outcome, ScrollAreaOutcome::Ignored) {
            scroll_area.handle_event(area, &mut state.scroll, event)
        } else {
            scroll_outcome
        };
        match scroll_outcome {
            ScrollAreaOutcome::Ignored => SetupBoardOutcome::Ignored,
            ScrollAreaOutcome::Handled => SetupBoardOutcome::Redraw,
            ScrollAreaOutcome::Scrolled { .. } | ScrollAreaOutcome::HorizontalScrolled { .. } => {
                SetupBoardOutcome::Panned
            }
        }
    }

    /// Render the virtual board into styled lines.
    #[must_use]
    pub fn render_lines(self, state: &SetupBoardState) -> Vec<Line> {
        let layout = self.layout();
        let mut surface = BoardSurface::new(layout.width, layout.height);
        for path in &layout.paths {
            path.render(&mut surface);
        }
        for spot in &layout.spots {
            BoardSpotComponent::new(spot).render(&mut surface, state);
        }
        surface.into_lines()
    }

    fn handle_mouse(
        self,
        area: Rect,
        state: &mut SetupBoardState,
        mouse: MouseEvent,
    ) -> Option<SetupBoardOutcome> {
        if !area.contains(mouse.position) {
            return None;
        }
        let relative_point = Point::new(
            mouse.position.x.saturating_sub(area.x),
            mouse.position.y.saturating_sub(area.y),
        );
        let virtual_point = Point::new(
            relative_point
                .x
                .saturating_add(state.scroll.horizontal_offset()),
            relative_point
                .y
                .saturating_add(state.scroll.vertical_offset()),
        );
        let layout = self.layout();
        let hit = layout.spot_at(virtual_point).map(|spot| spot.model.id);
        match mouse.kind {
            MouseEventKind::Move => {
                if state.hovered != hit {
                    state.hovered = hit;
                    return Some(SetupBoardOutcome::Redraw);
                }
                Some(SetupBoardOutcome::Ignored)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                state.pressed = hit;
                if let Some(id) = hit {
                    state.focused = id;
                    Some(SetupBoardOutcome::Focused(id))
                } else {
                    None
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = state.pressed.take();
                if let (Some(pressed), Some(hit)) = (pressed, hit)
                    && pressed == hit
                {
                    return Some(SetupBoardOutcome::Selected(hit));
                }
                Some(SetupBoardOutcome::Redraw)
            }
            MouseEventKind::Drag(MouseButton::Left) if state.pressed.is_some() => {
                state.pressed = None;
                None
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => None,
        }
    }

    fn layout(self) -> BoardLayout<'a> {
        BoardLayoutEngine::new(self.policy).layout(self.spots, self.connections)
    }
}

#[derive(Debug, Clone)]
struct BoardLayout<'a> {
    width: u16,
    height: u16,
    spots: Vec<LaidOutBoardSpot<'a>>,
    paths: Vec<LaidOutBoardPath>,
}

impl BoardLayout<'_> {
    fn spot_at(&self, point: Point) -> Option<&LaidOutBoardSpot<'_>> {
        self.spots.iter().find(|spot| spot.rect.contains(point))
    }
}

#[derive(Debug, Clone)]
struct LaidOutBoardSpot<'a> {
    model: &'a BoardSpot,
    rect: Rect,
}

#[derive(Debug, Clone, Copy)]
struct LaidOutBoardPath {
    from: Rect,
    to: Rect,
}

impl LaidOutBoardPath {
    fn render(self, surface: &mut BoardSurface) {
        let from = Point::new(
            self.from.x + self.from.width / 2,
            self.from.y + self.from.height - 1,
        );
        let to = Point::new(self.to.x + self.to.width / 2, self.to.y);
        let mid_y = from.y.saturating_add(to.y.saturating_sub(from.y) / 2);
        surface.draw_vertical(from.x, from.y.saturating_add(1), mid_y, "│", path_style());
        surface.draw_horizontal(from.x.min(to.x), from.x.max(to.x), mid_y, "─", path_style());
        surface.draw_vertical(to.x, mid_y, to.y.saturating_sub(1), "│", path_style());
        surface.put(to.x, to.y.saturating_sub(1), "▼", path_style());
        if from.x != to.x {
            surface.put(from.x, mid_y, "┴", path_style());
            surface.put(to.x, mid_y, "┬", path_style());
        }
    }
}

struct BoardLayoutEngine {
    policy: SetupBoardPolicy,
}

impl BoardLayoutEngine {
    const fn new(policy: SetupBoardPolicy) -> Self {
        Self { policy }
    }

    fn layout<'a>(
        &self,
        spots: &'a [BoardSpot],
        connections: &[BoardConnection],
    ) -> BoardLayout<'a> {
        let layers = Self::layers(spots, connections);
        let max_layer_width = layers
            .values()
            .map(|layer| self.layer_width(layer.len()))
            .max()
            .unwrap_or(self.policy.spot_width);
        let mut laid_out = Vec::new();
        let mut rects = BTreeMap::new();
        for (layer_index, layer) in &layers {
            let layer_width = self.layer_width(layer.len());
            let x_offset = self.policy.padding + max_layer_width.saturating_sub(layer_width) / 2;
            let y = self.policy.padding
                + layer_index.saturating_mul(self.policy.spot_height + self.policy.vertical_gap);
            for (index, spot) in layer.iter().enumerate() {
                let x = x_offset
                    + u16::try_from(index)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(self.policy.spot_width + self.policy.horizontal_gap);
                let rect = Rect::new(x, y, self.policy.spot_width, self.policy.spot_height);
                rects.insert(spot.id, rect);
                laid_out.push(LaidOutBoardSpot { model: spot, rect });
            }
        }
        let paths = connections
            .iter()
            .filter_map(|connection| {
                Some(LaidOutBoardPath {
                    from: *rects.get(&connection.from)?,
                    to: *rects.get(&connection.to)?,
                })
            })
            .collect::<Vec<_>>();
        let height = u16::try_from(layers.len())
            .unwrap_or(u16::MAX)
            .saturating_mul(self.policy.spot_height + self.policy.vertical_gap)
            .saturating_add(self.policy.padding);
        BoardLayout {
            width: max_layer_width.saturating_add(self.policy.padding.saturating_mul(2)),
            height,
            spots: laid_out,
            paths,
        }
    }

    fn layers<'a>(
        spots: &'a [BoardSpot],
        connections: &[BoardConnection],
    ) -> BTreeMap<u16, Vec<&'a BoardSpot>> {
        let mut hinted = BTreeMap::<u16, Vec<&BoardSpot>>::new();
        let mut unhinted = Vec::new();
        for spot in spots {
            if let Some(layer) = spot.hint.preferred_layer {
                hinted.entry(layer).or_default().push(spot);
            } else {
                unhinted.push(spot);
            }
        }
        let depths = graph_depths(spots, connections);
        for spot in unhinted {
            hinted
                .entry(*depths.get(&spot.id).unwrap_or(&0))
                .or_default()
                .push(spot);
        }
        hinted
    }

    fn layer_width(&self, spot_count: usize) -> u16 {
        let count = u16::try_from(spot_count.max(1)).unwrap_or(u16::MAX);
        count.saturating_mul(self.policy.spot_width).saturating_add(
            count
                .saturating_sub(1)
                .saturating_mul(self.policy.horizontal_gap),
        )
    }
}

fn graph_depths(
    spots: &[BoardSpot],
    connections: &[BoardConnection],
) -> BTreeMap<SetupSectionId, u16> {
    let all = spots.iter().map(|spot| spot.id).collect::<BTreeSet<_>>();
    let incoming = connections
        .iter()
        .map(|connection| connection.to)
        .collect::<BTreeSet<_>>();
    let roots = all.difference(&incoming).copied().collect::<Vec<_>>();
    let mut depths = BTreeMap::new();
    for root in roots {
        assign_depth(root, 0, connections, &mut depths);
    }
    depths
}

fn assign_depth(
    id: SetupSectionId,
    depth: u16,
    connections: &[BoardConnection],
    depths: &mut BTreeMap<SetupSectionId, u16>,
) {
    if depths.get(&id).is_some_and(|known| *known >= depth) {
        return;
    }
    depths.insert(id, depth);
    for child in connections
        .iter()
        .filter(|connection| connection.from == id)
    {
        assign_depth(child.to, depth.saturating_add(1), connections, depths);
    }
}

struct BoardSpotComponent<'a> {
    spot: &'a LaidOutBoardSpot<'a>,
}

impl<'a> BoardSpotComponent<'a> {
    const fn new(spot: &'a LaidOutBoardSpot<'a>) -> Self {
        Self { spot }
    }

    fn render(&self, surface: &mut BoardSurface, state: &SetupBoardState) {
        let rect = self.spot.rect;
        let model = self.spot.model;
        let style = spot_style(model, state);
        surface.draw_box(rect, style);
        surface.write_text(
            rect.x.saturating_add(2),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(4),
            &model.title,
            style,
        );
        surface.write_text(
            rect.x.saturating_add(2),
            rect.y.saturating_add(2),
            rect.width.saturating_sub(4),
            &model.subtitle,
            style,
        );
        let glyph = status_glyph(model.status);
        let glyph_x = rect.x.saturating_add(rect.width / 2).saturating_sub(1);
        surface.put(
            glyph_x,
            rect.y.saturating_add(rect.height).saturating_sub(1),
            glyph,
            style,
        );
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BoardCell {
    text: String,
    style: Style,
}

impl Default for BoardCell {
    fn default() -> Self {
        Self {
            text: " ".to_owned(),
            style: Style::new(),
        }
    }
}

struct BoardSurface {
    width: u16,
    height: u16,
    cells: Vec<BoardCell>,
}

impl BoardSurface {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![BoardCell::default(); usize::from(width) * usize::from(height)],
        }
    }

    fn put(&mut self, x: u16, y: u16, text: &str, style: Style) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = usize::from(y) * usize::from(self.width) + usize::from(x);
        self.cells[index] = BoardCell {
            text: text.to_owned(),
            style,
        };
    }

    fn write_text(&mut self, x: u16, y: u16, width: u16, text: &str, style: Style) {
        for (offset, ch) in text.chars().take(usize::from(width)).enumerate() {
            self.put(
                x.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
                y,
                &ch.to_string(),
                style,
            );
        }
    }

    fn draw_box(&mut self, rect: Rect, style: Style) {
        let right = rect.x.saturating_add(rect.width).saturating_sub(1);
        let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
        self.put(rect.x, rect.y, "╭", style);
        self.put(right, rect.y, "╮", style);
        self.put(rect.x, bottom, "╰", style);
        self.put(right, bottom, "╯", style);
        self.draw_horizontal(
            rect.x.saturating_add(1),
            right.saturating_sub(1),
            rect.y,
            "─",
            style,
        );
        self.draw_horizontal(
            rect.x.saturating_add(1),
            right.saturating_sub(1),
            bottom,
            "─",
            style,
        );
        self.draw_vertical(
            rect.x,
            rect.y.saturating_add(1),
            bottom.saturating_sub(1),
            "│",
            style,
        );
        self.draw_vertical(
            right,
            rect.y.saturating_add(1),
            bottom.saturating_sub(1),
            "│",
            style,
        );
    }

    fn draw_horizontal(&mut self, from_x: u16, to_x: u16, y: u16, text: &str, style: Style) {
        for x in from_x..=to_x {
            self.put(x, y, text, style);
        }
    }

    fn draw_vertical(&mut self, x: u16, from_y: u16, to_y: u16, text: &str, style: Style) {
        for y in from_y..=to_y {
            self.put(x, y, text, style);
        }
    }

    fn into_lines(self) -> Vec<Line> {
        (0..self.height)
            .map(|y| {
                let mut spans = Vec::new();
                let mut current_style = Style::new();
                let mut current = String::new();
                for x in 0..self.width {
                    let cell =
                        &self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)];
                    if !current.is_empty() && cell.style != current_style {
                        spans.push(Span::styled(std::mem::take(&mut current), current_style));
                    }
                    current_style = cell.style;
                    current.push_str(&cell.text);
                }
                if !current.is_empty() {
                    spans.push(Span::styled(current, current_style));
                }
                Line::from_spans(spans)
            })
            .collect()
    }
}

fn spot_style(spot: &BoardSpot, state: &SetupBoardState) -> Style {
    let base = match spot.status {
        SetupSectionStatus::Complete | SetupSectionStatus::Secured => Style::new().fg(Color::Green),
        SetupSectionStatus::Current => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        SetupSectionStatus::Recommended => Style::new().fg(Color::Yellow),
        SetupSectionStatus::Blocked | SetupSectionStatus::NeedsAttention => {
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        SetupSectionStatus::Visited => Style::new().fg(Color::Blue),
        SetupSectionStatus::Optional
        | SetupSectionStatus::Skipped
        | SetupSectionStatus::Unvisited => Style::new().fg(Color::BrightBlack),
    };
    let modifier = if state.pressed == Some(spot.id) {
        Some(Modifier::REVERSED)
    } else if state.focused == spot.id || state.hovered == Some(spot.id) {
        Some(Modifier::BOLD)
    } else {
        None
    };
    modifier.map_or(base, |modifier| base.add_modifier(modifier))
}

const fn path_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn status_glyph(status: SetupSectionStatus) -> &'static str {
    match status {
        SetupSectionStatus::Complete => "✓",
        SetupSectionStatus::Secured => "🔒",
        SetupSectionStatus::Current => "●",
        SetupSectionStatus::Visited => "◐",
        SetupSectionStatus::Recommended => "◆",
        SetupSectionStatus::Optional | SetupSectionStatus::Unvisited => "○",
        SetupSectionStatus::Skipped => "·",
        SetupSectionStatus::Blocked | SetupSectionStatus::NeedsAttention => "!",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rects_intersect(a: Rect, b: Rect) -> bool {
        let a_right = a.x.saturating_add(a.width);
        let b_right = b.x.saturating_add(b.width);
        let a_bottom = a.y.saturating_add(a.height);
        let b_bottom = b.y.saturating_add(b.height);
        a.x < b_right && b.x < a_right && a.y < b_bottom && b.y < a_bottom
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>()
    }

    #[test]
    fn layout_auto_spaces_spots_without_absolute_coordinates() {
        let spots = vec![
            BoardSpot::new(
                SetupSectionId::Welcome,
                "Welcome",
                "Base Camp",
                SetupSectionStatus::Current,
            )
            .layer(0),
            BoardSpot::new(
                SetupSectionId::Detection,
                "Detection",
                "Scout Tower",
                SetupSectionStatus::Complete,
            )
            .layer(1),
            BoardSpot::new(
                SetupSectionId::Providers,
                "Providers",
                "Signal Station",
                SetupSectionStatus::Recommended,
            )
            .layer(1),
        ];
        let connections = vec![
            BoardConnection::new(SetupSectionId::Welcome, SetupSectionId::Detection),
            BoardConnection::new(SetupSectionId::Welcome, SetupSectionId::Providers),
        ];

        let layout = SetupBoard::new(&spots, &connections).layout();

        assert_eq!(layout.spots.len(), 3);
        assert_eq!(layout.paths.len(), 2);
        assert!(!rects_intersect(layout.spots[1].rect, layout.spots[2].rect));
    }

    #[test]
    fn render_lines_include_spot_labels_and_status_glyphs() {
        let spots = vec![BoardSpot::new(
            SetupSectionId::SecureVault,
            "Secure Vault",
            "Lockbox",
            SetupSectionStatus::Secured,
        )];
        let state = SetupBoardState::new(SetupSectionId::SecureVault);
        let lines = SetupBoard::new(&spots, &[]).render_lines(&state);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("Secure Vault"));
        assert!(text.contains("Lockbox"));
        assert!(text.contains("🔒"));
    }

    #[test]
    fn dragging_from_spot_pans_without_losing_drag_anchor() {
        let spots = vec![BoardSpot::new(
            SetupSectionId::Welcome,
            "Welcome",
            "Base Camp",
            SetupSectionStatus::Current,
        )];
        let board = SetupBoard::new(&spots, &[]);
        let mut state = SetupBoardState::new(SetupSectionId::Welcome);
        let area = Rect::new(0, 0, 8, 4);

        assert_eq!(
            board.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(3, 3)
                )),
            ),
            SetupBoardOutcome::Focused(SetupSectionId::Welcome)
        );
        assert_eq!(
            board.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Drag(MouseButton::Left),
                    Point::new(1, 3)
                )),
            ),
            SetupBoardOutcome::Panned
        );
        assert_eq!(state.scroll.horizontal_offset(), 2);
    }

    #[test]
    fn click_position_uses_board_area_relative_coordinates() {
        let spots = vec![BoardSpot::new(
            SetupSectionId::Welcome,
            "Welcome",
            "Base Camp",
            SetupSectionStatus::Current,
        )];
        let board = SetupBoard::new(&spots, &[]);
        let mut state = SetupBoardState::new(SetupSectionId::Welcome);
        let area = Rect::new(10, 5, 40, 10);

        assert_eq!(
            board.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(13, 8)
                )),
            ),
            SetupBoardOutcome::Focused(SetupSectionId::Welcome)
        );
        assert_eq!(
            board.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(13, 8)
                )),
            ),
            SetupBoardOutcome::Selected(SetupSectionId::Welcome)
        );
    }

    #[test]
    fn clicking_spot_selects_it() {
        let spots = vec![BoardSpot::new(
            SetupSectionId::Welcome,
            "Welcome",
            "Base Camp",
            SetupSectionStatus::Current,
        )];
        let board = SetupBoard::new(&spots, &[]);
        let mut state = SetupBoardState::new(SetupSectionId::Welcome);
        let area = Rect::new(0, 0, 40, 10);

        assert!(
            board
                .handle_event(
                    area,
                    &mut state,
                    &Event::Mouse(MouseEvent::new(
                        MouseEventKind::Down(MouseButton::Left),
                        Point::new(3, 3)
                    )),
                )
                .is_handled()
        );
        assert_eq!(
            board.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(3, 3)
                )),
            ),
            SetupBoardOutcome::Selected(SetupSectionId::Welcome)
        );
    }
}
