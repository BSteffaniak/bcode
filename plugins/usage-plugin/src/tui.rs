//! BMUX-native usage surface. All source access goes through typed host capabilities.
mod timeline;

use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface,
    PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest, PluginTuiTheme,
};
use bcode_session_models::{SessionCostRange, SessionId, SessionUsageQuery};
use bcode_usage_models::{USAGE_VERSION, UsageQuery, UsageReport};
use bmux_keyboard::KeyCode;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::Size;
use bmux_tui::{
    event::Event,
    geometry::Rect,
    paint::{LocalRect, PaintCx},
    prelude::Line,
    style::Style,
};
use bmux_tui_components::bar_chart::{BarChartComponent, BarChartItem};
use bmux_tui_components::table::{Table, TableColumn, TableRow, TableState};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// Register the native usage dashboard.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(Factory));
    registry
}
struct Factory;
impl PluginTuiSurfaceFactory for Factory {
    fn surface_kind(&self) -> &'static str {
        "usage-dashboard"
    }
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            Ok(Box::new(Dashboard::new(request.session_id)) as BoxedPluginTuiSurface)
        })
    }
}

enum Update {
    Report(Result<UsageReport, String>),
    Collection(Result<bcode_session_models::SessionUsagePage, String>),
}
struct Dashboard {
    session: Option<SessionId>,
    query: UsageQuery,
    report: Option<UsageReport>,
    updates: Arc<Mutex<Option<Update>>>,
    busy: bool,
    status: String,
    collection: SessionUsageQuery,
    table: TableState,
    area: Rect,
    details: bool,
}
impl Dashboard {
    fn new(session: Option<SessionId>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(1)
            });
        Self {
            session,
            query: UsageQuery {
                version: USAGE_VERSION,
                range: SessionCostRange {
                    from_timestamp_ms: now.saturating_sub(30 * 86_400_000),
                    to_timestamp_ms: now,
                },
                models: BTreeSet::new(),
                providers: BTreeSet::new(),
                session_id: None,
                bucket_ms: 86_400_000,
                after: None,
                revision: None,
                limit: 256,
            },
            report: None,
            updates: Arc::new(Mutex::new(None)),
            busy: false,
            status: "r query snapshots | c collect active session (one page per press)".into(),
            collection: SessionUsageQuery {
                range: SessionCostRange {
                    from_timestamp_ms: 0,
                    to_timestamp_ms: i64::MAX.cast_unsigned(),
                },
                after: None,
                limit: 256,
                generation: None,
            },
            table: TableState::default(),
            area: Rect::new(0, 0, 0, 0),
            details: false,
        }
    }
    fn receive(&mut self) {
        let update = self.updates.lock().ok().and_then(|mut slot| slot.take());
        if let Some(update) = update {
            self.busy = false;
            match update {
                Update::Report(Ok(report)) => {
                    self.status.clone_from(&report.coverage);
                    self.report = Some(report);
                }
                Update::Collection(Ok(page)) => {
                    self.collection.after = page.next_after;
                    self.collection.generation = Some(page.generation);
                    self.status = if self.collection.after.is_some() {
                        "Collection incomplete: c next page"
                    } else {
                        "Session snapshot published: r query; c recollect"
                    }
                    .into();
                }
                Update::Report(Err(error)) | Update::Collection(Err(error)) => {
                    self.status = error;
                    self.collection.after = None;
                    self.collection.generation = None;
                }
            }
        }
    }
    fn refresh(&mut self, host: &dyn PluginTuiHost) {
        if self.busy {
            return;
        }
        self.busy = true;
        let future = host.usage_report(self.query.clone());
        let updates = Arc::clone(&self.updates);
        host.spawn(Box::pin(async move {
            let result = future
                .await
                .map_err(|_| "Query failed; r restart".to_owned());
            if let Ok(mut slot) = updates.lock() {
                *slot = Some(Update::Report(result));
            }
        }));
    }
    fn rows(&self) -> Vec<TableRow> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        if self.details {
            report
                .requests
                .iter()
                .map(|row| {
                    TableRow::rich(vec![
                        Line::from(row.session_id.to_string()),
                        Line::from(row.entry.key.clone()),
                        Line::from(format!("{:?}", row.entry.usage.cost)),
                    ])
                })
                .collect()
        } else {
            report
                .models
                .iter()
                .map(|row| {
                    TableRow::rich(vec![
                        Line::from(
                            row.model
                                .provider
                                .clone()
                                .unwrap_or_else(|| "Unknown".into()),
                        ),
                        Line::from(row.model.model.clone().unwrap_or_else(|| "Unknown".into())),
                        Line::from(format!(
                            "{:?} micros; {}/{} priced",
                            row.totals.cost_micros, row.totals.priced_requests, row.totals.requests
                        )),
                    ])
                })
                .collect()
        }
    }
    fn filter_key(&mut self, key: KeyCode, host: &dyn PluginTuiHost) -> bool {
        if self.busy {
            return false;
        }
        let selected = self.table.selected().unwrap_or(0);
        match key {
            KeyCode::Enter if self.details => {
                let Some(row) = self
                    .report
                    .as_ref()
                    .and_then(|report| report.requests.get(selected))
                else {
                    return false;
                };
                self.query.session_id = Some(row.session_id);
            }
            KeyCode::Enter => {
                let Some(row) = self
                    .report
                    .as_ref()
                    .and_then(|report| report.models.get(selected))
                else {
                    return false;
                };
                self.query.models = BTreeSet::from([row.model.clone()]);
            }
            KeyCode::Char('p') if !self.details => {
                let Some(provider) = self
                    .report
                    .as_ref()
                    .and_then(|report| report.models.get(selected))
                    .and_then(|row| row.model.provider.clone())
                else {
                    return false;
                };
                self.query.providers = BTreeSet::from([provider]);
            }
            KeyCode::Char('1' | '7' | '3') => {
                let days = match key {
                    KeyCode::Char('1') => 1,
                    KeyCode::Char('7') => 7,
                    _ => 30,
                };
                self.query.range.from_timestamp_ms = self
                    .query
                    .range
                    .to_timestamp_ms
                    .saturating_sub(days * 86_400_000);
            }
            KeyCode::Char('a') => {
                self.query.models.clear();
                self.query.providers.clear();
                self.query.session_id = None;
            }
            _ => return false,
        }
        self.query.after = None;
        self.query.revision = None;
        self.table = TableState::default();
        self.refresh(host);
        true
    }

    const fn columns(&self) -> [TableColumn<'static>; 3] {
        if self.details {
            [
                TableColumn::new("Session"),
                TableColumn::new("Request"),
                TableColumn::new("Estimate / provenance"),
            ]
        } else {
            [
                TableColumn::new("Provider"),
                TableColumn::new("Model"),
                TableColumn::new("Estimated costs / coverage"),
            ]
        }
    }
}
impl PluginTuiSurface for Dashboard {
    fn id(&self) -> &'static str {
        "usage-dashboard"
    }
    fn title(&self) -> &'static str {
        "Usage — estimated snapshot costs"
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        self.receive();
        self.area = Rect::new(
            area.x,
            area.y.saturating_add(area.height.min(3)),
            area.width,
            area.height.saturating_sub(3),
        );
        let heading = format!(
            "Usage | UTC {}..{} | PAGE SUBTOTALS",
            self.query.range.from_timestamp_ms, self.query.range.to_timestamp_ms
        );
        for (index, text) in [heading.as_str(), self.status.as_str(), "r refresh c collect n next Enter drill-down p provider a all 1/7/3 days d requests j JSON x CSV q close"].into_iter().enumerate() {
            let row = u16::try_from(index).unwrap_or_default();
            if row < area.height { frame.write_line_with_fallback_style(LocalRect::terminal(Rect::new(area.x, area.y + row, area.width, 1)), &Line::from(text), Style::new()); }
        }
        if !self.details
            && self.area.height >= 12
            && let Some(report) = &self.report
        {
            // Never compare mixed currencies on the same numeric axis.
            if report.totals.cost_micros.len() == 1
                && let Some(currency) = report.totals.cost_micros.keys().next()
            {
                let labels: Vec<_> = report
                    .models
                    .iter()
                    .take(5)
                    .map(|row| {
                        format!(
                            "{} {}",
                            row.model.model.as_deref().unwrap_or("Unknown"),
                            currency
                        )
                    })
                    .collect();
                let bars: Vec<_> = labels
                    .iter()
                    .zip(report.models.iter())
                    .map(|(label, row)| {
                        BarChartItem::new(
                            label,
                            row.totals
                                .cost_micros
                                .get(currency)
                                .copied()
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                let chart_area = Rect::new(self.area.x, self.area.y, self.area.width, 5);
                let chart = BarChartComponent::new("usage-model-costs", &bars);
                let layout = chart.layout(
                    Constraints::tight(Size::new(chart_area.width, chart_area.height)),
                    &mut LayoutCx::new(),
                );
                frame.with_child(
                    i32::from(chart_area.x),
                    i64::from(chart_area.y),
                    LocalRect::new(0, 0, chart_area.width, chart_area.height),
                    |cx| chart.paint(&layout, cx),
                );
                self.area.y = self.area.y.saturating_add(5);
                self.area.height = self.area.height.saturating_sub(5);
            }
        }
        if !self.details
            && self.area.height >= 12
            && let Some(report) = &self.report
        {
            let chart_area = Rect::new(self.area.x, self.area.y, self.area.width, 6);
            timeline::paint(report, chart_area, frame);
            self.area.y = self.area.y.saturating_add(6);
            self.area.height = self.area.height.saturating_sub(6);
        }
        Table::new(&self.columns(), &self.rows()).paint(self.area, &self.table, frame);
    }
    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut PaintCx<'_, '_>,
        theme: Option<&PluginTuiTheme>,
    ) {
        if let Some(theme) = theme {
            frame.fill(LocalRect::terminal(area), " ", theme.canvas);
        }
        self.render(area, frame);
    }
    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        self.receive();
        if let Event::Key(stroke) = event {
            if self.filter_key(stroke.key, host) {
                return PluginTuiAction::Redraw;
            }
            match stroke.key {
                KeyCode::Escape | KeyCode::Char('q') => {
                    return PluginTuiAction::Close { outcome: None };
                }
                KeyCode::Char('r') if !self.busy => {
                    self.query.after = None;
                    self.query.revision = None;
                    self.refresh(host);
                }
                KeyCode::Char('n') if !self.busy => {
                    if let Some(report) = &self.report
                        && report.next_after.is_some()
                    {
                        self.query.after = report.next_after;
                        self.query.revision = Some(report.revision);
                        self.refresh(host);
                    }
                }
                KeyCode::Char('c') if !self.busy => {
                    if let Some(session) = self.session {
                        let future = host.collect_usage(session, self.collection.clone());
                        let updates = Arc::clone(&self.updates);
                        self.busy = true;
                        host.spawn(Box::pin(async move {
                            let result = future
                                .await
                                .map_err(|_| "Collection failed; c restart".to_owned());
                            if let Ok(mut slot) = updates.lock() {
                                *slot = Some(Update::Collection(result));
                            }
                        }));
                    } else {
                        self.status = "Open /usage from a session to collect it".into();
                    }
                }
                KeyCode::Char('d') => {
                    self.details = !self.details;
                    self.table = TableState::default();
                }
                KeyCode::Char('j' | 'x') => {
                    if let Some(report) = &self.report {
                        let output = if stroke.key == KeyCode::Char('x') {
                            Ok(bcode_usage::export_csv(report))
                        } else {
                            bcode_usage::export_json(report).map_err(|_| "serialization failed")
                        };
                        self.status = match output {
                            Ok(text) => match host.copy_text(text) {
                                Ok(()) => "Copied this report page, including coverage".into(),
                                Err(_) => "Clipboard unavailable".into(),
                            },
                            Err(error) => error.into(),
                        };
                    }
                }
                _ => {
                    Table::new(&self.columns(), &self.rows()).handle_event(
                        self.area,
                        &mut self.table,
                        event,
                    );
                }
            }
        } else {
            Table::new(&self.columns(), &self.rows()).handle_event(
                self.area,
                &mut self.table,
                event,
            );
        }
        PluginTuiAction::Redraw
    }
}
