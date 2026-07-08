//! Plugin-owned persisted metrics dashboard surface.

use bcode_metrics::dashboard::{
    MetricDomain, MetricDomainSummary, MetricFilter, MetricFilterOp, MetricFilterTarget,
    MetricGroupBy, MetricSort, MetricSortDirection, MetricSortField, MetricsDashboardData,
    MetricsDashboardQuery, MetricsHealth, query_dashboard_report,
};
use bcode_metrics::{MetricsEventLogConfig, MetricsRegistry, MetricsReport};
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use bmux_tui_components::bar_chart::{BarChartItem, BarChartStyles};
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::sparkline::{Sparkline, SparklinePolicy, SparklineStyles};
use bmux_tui_components::tab_bar::{TabBar, TabBarOutcome, TabBarState, TabBarStyles, TabItem};
use bmux_tui_components::table::{
    Table, TableColumn, TableOutcome, TableRow, TableState, TableStyles,
};
use std::path::PathBuf;

const TITLE_HEIGHT: u16 = 2;
const TAB_HEIGHT: u16 = 1;
const ACTION_HEIGHT: u16 = 1;
const STATUS_HEIGHT: u16 = 1;
const CARD_HEIGHT: u16 = 4;

const BG: Color = Color::Rgb(8, 13, 20);
const PANEL: Color = Color::Rgb(15, 23, 34);
const PANEL_ALT: Color = Color::Rgb(20, 31, 45);
const BORDER: Color = Color::Rgb(51, 65, 85);
const ACCENT: Color = Color::Rgb(56, 189, 248);
const ACCENT_STRONG: Color = Color::Rgb(14, 165, 233);
const SUCCESS: Color = Color::Rgb(34, 197, 94);
const WARNING: Color = Color::Rgb(250, 204, 21);
const DANGER: Color = Color::Rgb(248, 113, 113);
const MUTED: Color = Color::Rgb(148, 163, 184);
const TEXT: Color = Color::Rgb(226, 232, 240);
const PURPLE: Color = Color::Rgb(168, 85, 247);

/// Persisted metrics dashboard surface.
#[derive(Debug)]
pub struct MetricsDashboardSurface {
    metrics_path: PathBuf,
    report: MetricsReport,
    dashboard: MetricsDashboardData,
    tab_state: TabBarState,
    row_state: TableState,
    recommendation_state: TableState,
    action_state: ActionRowState,
    status: String,
    query: MetricsDashboardQuery,
    facets: Vec<bcode_metrics::dashboard::MetricFacet>,
    total_events: usize,
    filtered_events: usize,
    tab_area: Rect,
    content_area: Rect,
    action_area: Rect,
    main_table_area: Rect,
    recommendation_area: Rect,
}

/// Parse generic dashboard query options, accepting legacy `session_id` as a label filter.
#[must_use]
pub fn dashboard_query_from_options(options: &serde_json::Value) -> MetricsDashboardQuery {
    let mut query = options
        .get("query")
        .cloned()
        .and_then(|value| serde_json::from_value::<MetricsDashboardQuery>(value).ok())
        .unwrap_or_default();
    if let Some(session_id) = options
        .get("session_id")
        .and_then(serde_json::Value::as_str)
    {
        query.filters.push(MetricFilter {
            target: MetricFilterTarget::Label("session_id".to_owned()),
            op: MetricFilterOp::Equals,
            value: Some(session_id.to_owned()),
        });
    }
    query
}

impl MetricsDashboardSurface {
    #[must_use]
    pub fn load(metrics_path: PathBuf, query: MetricsDashboardQuery) -> Self {
        let (report, query_result, status) = load_report(&metrics_path, &query);
        Self {
            metrics_path,
            report,
            dashboard: query_result.dashboard,
            tab_state: TabBarState::new(Some(0)),
            row_state: TableState::new(Some(0)),
            recommendation_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            status,
            query,
            facets: query_result.facets,
            total_events: query_result.total_events,
            filtered_events: query_result.filtered_events,
            tab_area: Rect::new(0, 0, 0, 0),
            content_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
            main_table_area: Rect::new(0, 0, 0, 0),
            recommendation_area: Rect::new(0, 0, 0, 0),
        }
    }

    fn reload(&mut self) {
        let (report, query_result, status) = load_report(&self.metrics_path, &self.query);
        self.report = report;
        self.dashboard = query_result.dashboard;
        self.facets = query_result.facets;
        self.total_events = query_result.total_events;
        self.filtered_events = query_result.filtered_events;
        self.status = status;
        self.row_state = TableState::new(Some(0));
        self.recommendation_state = TableState::new(Some(0));
    }

    fn selected_domain(&self) -> MetricDomain {
        domain_from_index(self.tab_state.selected().unwrap_or(0))
    }

    fn selected_summary(&self) -> Option<&MetricDomainSummary> {
        let domain = self.selected_domain();
        self.dashboard
            .domains
            .iter()
            .find(|summary| summary.domain == domain)
    }

    fn handle_action(&mut self, action: &str) -> PluginTuiAction {
        match action {
            "refresh" => {
                self.reload();
                PluginTuiAction::Redraw
            }
            "overview" => {
                self.tab_state.set_selected(Some(0));
                PluginTuiAction::Redraw
            }
            "sort" => {
                self.cycle_sort();
                PluginTuiAction::Redraw
            }
            "group" => {
                self.cycle_group_by();
                PluginTuiAction::Redraw
            }
            "filter" => {
                self.filter_by_selected_row_label();
                PluginTuiAction::Redraw
            }
            "clear" => {
                self.query.filters.clear();
                self.reload();
                PluginTuiAction::Redraw
            }
            "close" => PluginTuiAction::Close { outcome: None },
            _ => PluginTuiAction::None,
        }
    }

    fn render_dashboard(&mut self, area: Rect, frame: &mut Frame<'_>) {
        render_header(area, frame, "Metrics Dashboard", &self.status);
        self.tab_area = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT),
            area.width,
            TAB_HEIGHT,
        );
        TabBar::new(&dashboard_tabs())
            .styles(metric_tab_styles())
            .render(self.tab_area, &self.tab_state, frame);
        let body = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT + TAB_HEIGHT),
            area.width,
            area.height.saturating_sub(TITLE_HEIGHT + TAB_HEIGHT),
        );
        let (content_area, action_area, status_area) = split_body_actions(body);
        self.content_area = content_area;
        self.action_area = action_area;
        self.render_domain(content_area, frame);
        themed_action_row(&dashboard_actions()).render_state(
            action_area,
            &self.action_state,
            frame,
        );
        render_status(
            status_area,
            frame,
            "Mouse: click tabs/rows/buttons. Keys: r refresh, s sort, d direction, g group, f filter row, c clear, 1-8 tabs, q close.",
        );
    }

    fn render_domain(&mut self, area: Rect, frame: &mut Frame<'_>) {
        let Some(summary) = self.selected_summary().cloned() else {
            render_panel_title(area, frame, "No metrics loaded");
            return;
        };
        render_panel_title(
            area,
            frame,
            &format!(
                "{} command center | {} / {} events | filters: {} | group: {} | sort: {}",
                domain_title(summary.domain),
                self.filtered_events,
                self.total_events,
                if self.query.filters.is_empty() {
                    "none".to_owned()
                } else {
                    self.query
                        .filters
                        .iter()
                        .map(filter_label)
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                group_label(&self.query.group_by),
                sort_label(&self.query.sort)
            ),
        );
        let inner = inset_top(area, 1);
        let card_area = Rect::new(inner.x, inner.y, inner.width, CARD_HEIGHT);
        render_cards(frame, card_area, &summary.cards);
        let below_cards_y = inner.y.saturating_add(CARD_HEIGHT).saturating_add(1);
        let below_cards = Rect::new(
            inner.x,
            below_cards_y,
            inner.width,
            inner.height.saturating_sub(CARD_HEIGHT).saturating_sub(1),
        );
        let rows = split_rows(below_cards, 2, 1);
        if let Some(top) = rows.first().copied() {
            let columns = split_columns(top, 2, 2);
            if let Some(left) = columns.first().copied() {
                self.render_series(left, frame, &summary);
            }
            if let Some(right) = columns.get(1).copied() {
                self.render_facets(right, frame);
            }
        }
        if let Some(bottom) = rows.get(1).copied() {
            self.main_table_area = bottom;
            self.render_main_table(bottom, frame, &summary);
        }
    }

    fn render_series(&self, area: Rect, frame: &mut Frame<'_>, summary: &MetricDomainSummary) {
        render_panel_title(area, frame, "Recent timeline");
        let area = inset_top(area, 1);
        let Some(series) = summary.series.first() else {
            return;
        };
        let spark = Sparkline::new(&series.points)
            .policy(SparklinePolicy::default())
            .styles(metric_sparkline_styles());
        spark.render(area, frame);
    }

    fn render_facets(&mut self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Label facets");
        let rows = self
            .facets
            .iter()
            .flat_map(|facet| {
                facet.values.iter().take(3).map(|value| {
                    table_row(vec![
                        facet.key.clone(),
                        value.value.clone(),
                        value.count.to_string(),
                    ])
                })
            })
            .take(12)
            .collect::<Vec<_>>();
        let columns = vec![
            TableColumn::new("Label").fixed(20),
            TableColumn::new("Value").fixed(38),
            TableColumn::new("Count").fixed(8),
        ];
        self.recommendation_area = inset_top(area, 1);
        render_metric_table(
            frame,
            self.recommendation_area,
            &columns,
            &rows,
            &self.recommendation_state,
        );
    }

    fn render_main_table(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        summary: &MetricDomainSummary,
    ) {
        render_panel_title(area, frame, "Metric groups");
        let rows = summary
            .rows
            .iter()
            .map(|row| {
                table_row(vec![
                    row.metric.clone(),
                    row.group.clone(),
                    row.count.to_string(),
                    format_metric_value(&row.metric, row.average),
                    format_metric_value(&row.metric, row.max),
                ])
            })
            .collect::<Vec<_>>();
        let columns = vec![
            TableColumn::new("Metric").fixed(34),
            TableColumn::new("Labels").fixed(44),
            TableColumn::new("Count").fixed(10),
            TableColumn::new("Avg").fixed(10),
            TableColumn::new("Max").fixed(10),
        ];
        self.main_table_area = inset_top(area, 1);
        render_metric_table(
            frame,
            self.main_table_area,
            &columns,
            &rows,
            &self.row_state,
        );
    }

    fn selected_row(&self) -> Option<bcode_metrics::dashboard::MetricTableRow> {
        let selected = self.row_state.selected()?;
        self.selected_summary()?.rows.get(selected).cloned()
    }

    fn filter_by_selected_row_label(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let Some((key, value)) = row.labels.iter().next() else {
            return;
        };
        self.query.filters.push(MetricFilter {
            target: MetricFilterTarget::Label(key.clone()),
            op: MetricFilterOp::Equals,
            value: Some(value.clone()),
        });
        self.reload();
    }

    fn cycle_sort(&mut self) {
        self.query.sort.field = match &self.query.sort.field {
            MetricSortField::Max => MetricSortField::Count,
            MetricSortField::Count => MetricSortField::Average,
            MetricSortField::Average => MetricSortField::LastSeen,
            MetricSortField::LastSeen => MetricSortField::Metric,
            MetricSortField::Metric => MetricSortField::Group,
            MetricSortField::Group | MetricSortField::Label(_) => MetricSortField::Max,
        };
        self.reload();
    }

    fn toggle_sort_direction(&mut self) {
        self.query.sort.direction = match self.query.sort.direction {
            MetricSortDirection::Asc => MetricSortDirection::Desc,
            MetricSortDirection::Desc => MetricSortDirection::Asc,
        };
        self.reload();
    }

    fn cycle_group_by(&mut self) {
        self.query.group_by = match &self.query.group_by {
            MetricGroupBy::MetricAndLabels => MetricGroupBy::Metric,
            MetricGroupBy::Metric => self.facets.first().map_or(MetricGroupBy::Domain, |facet| {
                MetricGroupBy::Label(facet.key.clone())
            }),
            MetricGroupBy::Label(_) => MetricGroupBy::Domain,
            MetricGroupBy::Domain | MetricGroupBy::Labels(_) => MetricGroupBy::MetricAndLabels,
        };
        self.reload();
    }

    fn handle_tables(&mut self, event: &Event) -> bool {
        let Some(summary) = self.selected_summary().cloned() else {
            return false;
        };
        let main_rows = summary
            .rows
            .iter()
            .map(|row| table_row(vec![row.metric.clone(), row.group.clone()]))
            .collect::<Vec<_>>();
        let main_columns = vec![
            TableColumn::new("Metric").fixed(34),
            TableColumn::new("Labels").fixed(44),
        ];
        if handle_metric_table_event(
            self.main_table_area,
            &main_columns,
            &main_rows,
            &mut self.row_state,
            event,
        ) {
            return true;
        }
        let recommendation_rows = summary
            .recommendations
            .iter()
            .map(|recommendation| table_row(vec![recommendation.title.clone()]))
            .collect::<Vec<_>>();
        let recommendation_columns = vec![TableColumn::new("Finding").fixed(40)];
        handle_metric_table_event(
            self.recommendation_area,
            &recommendation_columns,
            &recommendation_rows,
            &mut self.recommendation_state,
            event,
        )
    }

    fn handle_key_event(&mut self, event: &Event) -> Option<PluginTuiAction> {
        let Event::Key(stroke) = event else {
            return None;
        };
        match stroke.key {
            KeyCode::Char('q') | KeyCode::Escape => Some(PluginTuiAction::Close { outcome: None }),
            KeyCode::Char('r') => {
                self.reload();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('s') => {
                self.cycle_sort();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('d') => {
                self.toggle_sort_direction();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('g') => {
                self.cycle_group_by();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('f') => {
                self.filter_by_selected_row_label();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('c') => {
                self.query.filters.clear();
                self.reload();
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('1') => {
                self.tab_state.set_selected(Some(0));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('2') => {
                self.tab_state.set_selected(Some(1));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('3') => {
                self.tab_state.set_selected(Some(2));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('4') => {
                self.tab_state.set_selected(Some(3));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('5') => {
                self.tab_state.set_selected(Some(4));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('6') => {
                self.tab_state.set_selected(Some(5));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('7') => {
                self.tab_state.set_selected(Some(6));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('8') => {
                self.tab_state.set_selected(Some(7));
                Some(PluginTuiAction::Redraw)
            }
            _ => None,
        }
    }
}

impl PluginTuiSurface for MetricsDashboardSurface {
    fn id(&self) -> &'static str {
        "bcode.metrics-dashboard"
    }

    fn title(&self) -> &'static str {
        "Metrics Dashboard"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.render_dashboard(area, frame);
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        match TabBar::new(&dashboard_tabs())
            .styles(metric_tab_styles())
            .handle_event(self.tab_area, &mut self.tab_state, event)
        {
            TabBarOutcome::Selected(_) | TabBarOutcome::Redraw => return PluginTuiAction::Redraw,
            TabBarOutcome::Ignored => {}
        }
        if self.handle_tables(event) {
            return PluginTuiAction::Redraw;
        }
        match themed_action_row(&dashboard_actions()).handle_event(
            self.action_area,
            &mut self.action_state,
            event,
        ) {
            ActionRowOutcome::Activated { id, .. } => return self.handle_action(&id),
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Some(action) = self.handle_mouse_scroll(event) {
            return action;
        }
        if let Some(action) = self.handle_key_event(event) {
            return action;
        }
        PluginTuiAction::None
    }
}

impl MetricsDashboardSurface {
    fn handle_mouse_scroll(&mut self, event: &Event) -> Option<PluginTuiAction> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                let next = self.row_state.selected().unwrap_or(0).saturating_add(1);
                self.row_state.set_selected(Some(next));
                Some(PluginTuiAction::Redraw)
            }
            MouseEventKind::ScrollUp => {
                let previous = self.row_state.selected().unwrap_or(0).saturating_sub(1);
                self.row_state.set_selected(Some(previous));
                Some(PluginTuiAction::Redraw)
            }
            _ => None,
        }
    }
}

fn load_report(
    path: &PathBuf,
    query: &MetricsDashboardQuery,
) -> (
    MetricsReport,
    bcode_metrics::dashboard::MetricsDashboardQueryResult,
    String,
) {
    let report =
        MetricsRegistry::report_from_event_log_path(path, MetricsEventLogConfig::default(), 20_000);
    let query_result = query_dashboard_report(&report, query);
    let status = format!(
        "{} / {} events  {} metrics  filters={} group={} sort={}  source={}",
        query_result.filtered_events,
        query_result.total_events,
        query_result.report.descriptors.len(),
        query.filters.len(),
        group_label(&query.group_by),
        sort_label(&query.sort),
        path.display()
    );
    (report, query_result, status)
}

fn sort_label(sort: &MetricSort) -> String {
    let field = match &sort.field {
        MetricSortField::Metric => "metric".to_owned(),
        MetricSortField::Group => "group".to_owned(),
        MetricSortField::Count => "count".to_owned(),
        MetricSortField::Average => "avg".to_owned(),
        MetricSortField::Max => "max".to_owned(),
        MetricSortField::LastSeen => "last_seen".to_owned(),
        MetricSortField::Label(key) => format!("label.{key}"),
    };
    let direction = match sort.direction {
        MetricSortDirection::Asc => "asc",
        MetricSortDirection::Desc => "desc",
    };
    format!("{field}:{direction}")
}

fn group_label(group_by: &MetricGroupBy) -> String {
    match group_by {
        MetricGroupBy::Metric => "metric".to_owned(),
        MetricGroupBy::MetricAndLabels => "metric+labels".to_owned(),
        MetricGroupBy::Label(key) => format!("label.{key}"),
        MetricGroupBy::Labels(keys) => format!("labels.{}", keys.join(",")),
        MetricGroupBy::Domain => "domain".to_owned(),
    }
}

fn filter_label(filter: &MetricFilter) -> String {
    let target = match &filter.target {
        MetricFilterTarget::Metric => "metric".to_owned(),
        MetricFilterTarget::Kind => "kind".to_owned(),
        MetricFilterTarget::Domain => "domain".to_owned(),
        MetricFilterTarget::Value => "value".to_owned(),
        MetricFilterTarget::Label(key) => key.clone(),
    };
    let op = match filter.op {
        MetricFilterOp::Equals => "=",
        MetricFilterOp::NotEquals => "!=",
        MetricFilterOp::Contains => "~",
        MetricFilterOp::Exists => " exists",
        MetricFilterOp::Missing => " missing",
        MetricFilterOp::GreaterThan => ">",
        MetricFilterOp::LessThan => "<",
    };
    filter.value.as_ref().map_or_else(
        || format!("{target}{op}"),
        |value| format!("{target}{op}{value}"),
    )
}
fn dashboard_tabs() -> Vec<TabItem<'static>> {
    vec![
        TabItem::new("overview", "Overview"),
        TabItem::new("providers", "Providers"),
        TabItem::new("tools", "Tools"),
        TabItem::new("plugins", "Plugins"),
        TabItem::new("sessions", "Sessions"),
        TabItem::new("ipc", "IPC"),
        TabItem::new("storage", "Storage"),
        TabItem::new("raw", "Raw"),
    ]
}

const fn domain_from_index(index: usize) -> MetricDomain {
    match index {
        0 => MetricDomain::Overview,
        1 => MetricDomain::Provider,
        2 => MetricDomain::Tool,
        3 => MetricDomain::Plugin,
        4 => MetricDomain::Session,
        5 => MetricDomain::Ipc,
        6 => MetricDomain::Storage,
        _ => MetricDomain::Raw,
    }
}

const fn domain_title(domain: MetricDomain) -> &'static str {
    match domain {
        MetricDomain::Overview => "Overview",
        MetricDomain::Provider => "Model Providers",
        MetricDomain::Tool => "Tools",
        MetricDomain::Plugin => "Plugins",
        MetricDomain::Session => "Sessions",
        MetricDomain::Ipc => "IPC",
        MetricDomain::Storage => "Metrics Storage",
        MetricDomain::Runtime => "Runtime",
        MetricDomain::Raw => "Raw Metrics",
    }
}

fn dashboard_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("refresh", "Refresh"),
        ActionButton::new("sort", "Sort"),
        ActionButton::new("group", "Group"),
        ActionButton::new("filter", "Filter Row"),
        ActionButton::new("clear", "Clear Filters"),
        ActionButton::new("overview", "Overview"),
        ActionButton::new("close", "Close"),
    ]
}

fn themed_action_row<'a>(actions: &'a [ActionButton]) -> ActionRow<'a> {
    ActionRow::new(actions).styles(metric_button_styles())
}

fn render_cards(frame: &mut Frame<'_>, area: Rect, cards: &[bcode_metrics::dashboard::MetricCard]) {
    let rects = split_columns(area, 4, 1);
    for (index, card) in cards.iter().take(4).enumerate() {
        if let Some(rect) = rects.get(index).copied() {
            render_kpi_card(
                frame,
                rect,
                &card.title,
                &card.value,
                &card.detail,
                health_color(card.health),
            );
        }
    }
}

fn render_kpi_card(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    detail: &str,
    color: Color,
) {
    if area.height == 0 {
        return;
    }
    fill_rect(frame, area, PANEL_ALT);
    frame.write_line_with_fallback_style(
        Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            1,
        ),
        &Line::from_spans(vec![Span::styled(
            label,
            Style::new()
                .fg(MUTED)
                .bg(PANEL_ALT)
                .add_modifier(Modifier::BOLD),
        )]),
        Style::new().bg(PANEL_ALT),
    );
    if area.height > 1 {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1),
                area.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                value,
                Style::new()
                    .fg(color)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            )]),
            Style::new().bg(PANEL_ALT),
        );
    }
    if area.height > 2 {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(2),
                area.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(
                detail,
                Style::new().fg(TEXT).bg(PANEL_ALT),
            )]),
            Style::new().bg(PANEL_ALT),
        );
    }
}

fn render_header(area: Rect, frame: &mut Frame<'_>, title: &str, status: &str) {
    fill_rect(frame, area, BG);
    let title_line = Line::from_spans(vec![
        Span::styled("  ", Style::new().bg(BG)),
        Span::styled(
            title,
            Style::new().fg(ACCENT).bg(BG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::new().bg(BG)),
        Span::styled(status, Style::new().fg(MUTED).bg(BG)),
    ]);
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &title_line,
        Style::new().bg(BG),
    );
    if area.height > 1 {
        frame.write_line_with_fallback_style(
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
            &Line::from_spans(vec![Span::styled(
                "─".repeat(usize::from(area.width)),
                Style::new().fg(BORDER).bg(BG),
            )]),
            Style::new().bg(BG),
        );
    }
}

fn render_status(area: Rect, frame: &mut Frame<'_>, text: &str) {
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(BG)),
            Span::styled(text, Style::new().fg(MUTED).bg(BG)),
        ]),
        Style::new().bg(BG),
    );
}

fn render_panel_title(area: Rect, frame: &mut Frame<'_>, title: &str) {
    if area.height == 0 {
        return;
    }
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![
            Span::styled(
                " ▸ ",
                Style::new()
                    .fg(ACCENT)
                    .bg(PANEL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                Style::new().fg(TEXT).bg(PANEL).add_modifier(Modifier::BOLD),
            ),
        ]),
        Style::new().bg(PANEL),
    );
}

fn table_row(cells: Vec<String>) -> TableRow {
    TableRow::rich(cells.into_iter().map(Line::from).collect::<Vec<_>>())
}

fn metric_table<'a>(columns: &'a [TableColumn<'a>], rows: &'a [TableRow]) -> Table<'a> {
    Table::new(columns, rows).styles(metric_table_styles())
}

fn render_metric_table(
    frame: &mut Frame<'_>,
    area: Rect,
    columns: &[TableColumn<'_>],
    rows: &[TableRow],
    state: &TableState,
) {
    metric_table(columns, rows).render(area, state, frame);
}

fn handle_metric_table_event(
    area: Rect,
    columns: &[TableColumn<'_>],
    rows: &[TableRow],
    state: &mut TableState,
    event: &Event,
) -> bool {
    table_action(metric_table(columns, rows).handle_event(area, state, event))
}

fn table_action(outcome: TableOutcome) -> bool {
    !matches!(outcome, TableOutcome::Ignored)
}

fn split_body_actions(area: Rect) -> (Rect, Rect, Rect) {
    let status_y = area.bottom().saturating_sub(STATUS_HEIGHT);
    let action_y = status_y.saturating_sub(ACTION_HEIGHT);
    (
        Rect::new(area.x, area.y, area.width, action_y.saturating_sub(area.y)),
        Rect::new(area.x, action_y, area.width, ACTION_HEIGHT),
        Rect::new(area.x, status_y, area.width, STATUS_HEIGHT),
    )
}

fn split_columns(area: Rect, columns: u16, gap: u16) -> Vec<Rect> {
    if columns == 0 {
        return Vec::new();
    }
    let total_gap = gap.saturating_mul(columns.saturating_sub(1));
    let width = area.width.saturating_sub(total_gap) / columns;
    (0..columns)
        .map(|index| {
            let x = area
                .x
                .saturating_add(index.saturating_mul(width.saturating_add(gap)));
            Rect::new(
                x,
                area.y,
                if index + 1 == columns {
                    area.right().saturating_sub(x)
                } else {
                    width
                },
                area.height,
            )
        })
        .collect()
}

fn split_rows(area: Rect, rows: u16, gap: u16) -> Vec<Rect> {
    if rows == 0 {
        return Vec::new();
    }
    let total_gap = gap.saturating_mul(rows.saturating_sub(1));
    let height = area.height.saturating_sub(total_gap) / rows;
    (0..rows)
        .map(|index| {
            let y = area
                .y
                .saturating_add(index.saturating_mul(height.saturating_add(gap)));
            Rect::new(
                area.x,
                y,
                area.width,
                if index + 1 == rows {
                    area.bottom().saturating_sub(y)
                } else {
                    height
                },
            )
        })
        .collect()
}

fn inset_top(area: Rect, top: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(top),
        area.width,
        area.height.saturating_sub(top),
    )
}

fn fill_rect(frame: &mut Frame<'_>, area: Rect, color: Color) {
    for y in area.y..area.bottom() {
        frame.write_line_with_fallback_style(
            Rect::new(area.x, y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                " ".repeat(usize::from(area.width)),
                Style::new().bg(color),
            )]),
            Style::new().bg(color),
        );
    }
}

const fn metric_table_styles() -> TableStyles {
    TableStyles {
        header: Style::new()
            .fg(ACCENT)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD),
        row: Style::new().fg(TEXT).bg(PANEL),
        selected: Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        selected_column: Style::new().fg(Color::Black).bg(ACCENT_STRONG),
        selected_cell: Style::new()
            .fg(Color::Black)
            .bg(WARNING)
            .add_modifier(Modifier::BOLD),
        hovered: Style::new().fg(Color::White).bg(PANEL_ALT),
        disabled: Style::new().fg(MUTED).bg(PANEL),
        separator: Style::new().fg(BORDER).bg(PANEL),
        empty: Style::new().fg(MUTED).bg(PANEL),
    }
}

const fn metric_tab_styles() -> TabBarStyles {
    TabBarStyles {
        normal: Style::new().fg(MUTED).bg(BG),
        selected: Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        focused: Style::new()
            .fg(TEXT)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::UNDERLINE),
        hovered: Style::new().fg(TEXT).bg(PANEL_ALT),
        pressed: Style::new()
            .fg(Color::Black)
            .bg(ACCENT_STRONG)
            .add_modifier(Modifier::BOLD),
        disabled: Style::new().fg(BORDER).bg(BG),
        separator: Style::new().fg(BORDER).bg(BG),
    }
}

const fn metric_button_styles() -> ButtonStyles {
    ButtonStyles {
        normal: Style::new().fg(TEXT).bg(PANEL_ALT),
        hovered: Style::new().fg(Color::Black).bg(ACCENT),
        pressed: Style::new()
            .fg(Color::Black)
            .bg(ACCENT_STRONG)
            .add_modifier(Modifier::BOLD),
        focused: Style::new()
            .fg(TEXT)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::UNDERLINE),
        disabled: Style::new().fg(MUTED).bg(BG),
    }
}

const fn metric_sparkline_styles() -> SparklineStyles {
    SparklineStyles {
        normal: Style::new().fg(ACCENT).bg(PANEL_ALT),
        latest: Style::new()
            .fg(WARNING)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::BOLD),
        first: Style::new().fg(PURPLE).bg(PANEL_ALT),
        high: Style::new()
            .fg(SUCCESS)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::BOLD),
        low: Style::new().fg(DANGER).bg(PANEL_ALT),
        empty: Style::new().fg(MUTED).bg(PANEL_ALT),
        background: Style::new().bg(PANEL_ALT),
    }
}

#[allow(dead_code)]
const fn metric_bar_chart_styles() -> BarChartStyles {
    BarChartStyles {
        label: Style::new().fg(TEXT).bg(PANEL),
        bar: Style::new()
            .fg(ACCENT)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD),
        empty: Style::new().fg(BORDER).bg(PANEL),
        value: Style::new().fg(MUTED).bg(PANEL),
        empty_message: Style::new().fg(MUTED).bg(PANEL),
    }
}

#[allow(dead_code)]
fn bar_items(rows: &[bcode_metrics::dashboard::MetricTableRow]) -> Vec<BarChartItem<'_>> {
    rows.iter()
        .take(8)
        .map(|row| BarChartItem::new(row.metric.as_str(), row.max))
        .collect()
}

const fn health_color(health: MetricsHealth) -> Color {
    match health {
        MetricsHealth::Good => SUCCESS,
        MetricsHealth::Warning => WARNING,
        MetricsHealth::Critical => DANGER,
    }
}

fn format_metric_value(metric: &str, value: u64) -> String {
    if metric.ends_with("duration_ms") {
        format_duration(value)
    } else if metric.ends_with("bytes")
        || metric.ends_with("payload_bytes")
        || metric.ends_with("output_bytes")
    {
        format_bytes(value)
    } else {
        format_count(value)
    }
}

fn format_duration(value: u64) -> String {
    if value >= 1_000 {
        format!("{}.{}s", value / 1_000, (value % 1_000) / 100)
    } else {
        format!("{value}ms")
    }
}

fn format_bytes(value: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if value >= MIB {
        format!("{}.{}MiB", value / MIB, (value % MIB) / (MIB / 10))
    } else if value >= KIB {
        format!("{}.{}KiB", value / KIB, (value % KIB) / (KIB / 10))
    } else {
        format!("{value}B")
    }
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}.{}m", value / 1_000_000, (value % 1_000_000) / 100_000)
    } else if value >= 1_000 {
        format!("{}.{}k", value / 1_000, (value % 1_000) / 100)
    } else {
        value.to_string()
    }
}
