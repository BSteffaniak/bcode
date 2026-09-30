//! Read-only, run-pinned supervision using existing bounded workflow projections.
use super::{
    Arc, BcodeClient, BoxedPluginTuiSurface, Event, FromStr, KeyCode, Mutex, PaintCx,
    PluginTuiAction, PluginTuiHost, PluginTuiSurface, PluginTuiSurfaceFactory,
    PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest, Rect, SessionId, workflow_binding_key,
};
use bcode_workflow_view_models::WorkflowRunView;
use std::time::{Duration, Instant};

pub const SURFACE: &str = "goal.watch";
pub struct Factory;
type Completion = Arc<Mutex<Option<Result<Option<WorkflowRunView>, String>>>>;

impl PluginTuiSurfaceFactory for Factory {
    fn surface_kind(&self) -> &'static str {
        SURFACE
    }
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let session = request.session_id.or_else(|| {
                request
                    .options
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| SessionId::from_str(s).ok())
            });
            let mut view = crate::goal_live::GenerationView::default();
            view.title = "Goal · Supervision";
            view.footer = "1–9 inspect execution session and return · Esc returns to conversation (does not stop work)";
            view.detail = "Connecting to the associated execution".into();
            Ok(Box::new(Supervision {
                session,
                run: request
                    .options
                    .get("run_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                pending: false,
                completion: Arc::default(),
                next_refresh: Instant::now(),
                snapshot: None,
                view,
                text: String::new(),
            }) as BoxedPluginTuiSurface)
        })
    }
}

struct Supervision {
    session: Option<SessionId>,
    run: Option<String>,
    pending: bool,
    completion: Completion,
    next_refresh: Instant,
    snapshot: Option<WorkflowRunView>,
    view: crate::goal_live::GenerationView,
    text: String,
}

fn content(snapshot: &WorkflowRunView) -> String {
    let mut text = crate::goal_status::overview(snapshot);
    text.push_str(&crate::goal_status::format(snapshot));
    text.push_str("\n\nExecution sessions · inspect without approving or cancelling\n");
    for (index, child) in snapshot.child_sessions.iter().take(9).enumerate() {
        use std::fmt::Write as _;
        let _ = writeln!(
            text,
            "{} · {} · attempt {} · {}",
            index + 1,
            child.node_id,
            child.attempt,
            child.session_id
        );
    }
    if snapshot.child_sessions.is_empty() {
        text.push_str("No execution sessions in this bounded snapshot.\n");
    }
    text.push_str("\n/workflow provides advanced controls and exact pending decisions.\nAvailable results above are observations, not independent verification.");
    text
}

impl PluginTuiSurface for Supervision {
    fn id(&self) -> &'static str {
        SURFACE
    }
    fn title(&self) -> &'static str {
        "Goal supervision"
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        crate::goal::paint_generation(&mut self.view, area, frame);
    }
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(result) = self.completion.lock().expect("supervision result").take() {
            self.pending = false;
            self.next_refresh = Instant::now() + Duration::from_secs(2);
            match result {
                Ok(Some(snapshot))
                    if snapshot.version == bcode_workflow_view_models::WORKFLOW_VIEW_VERSION =>
                {
                    self.run = Some(snapshot.run.run_id.clone());
                    self.view.detail = format!(
                        "Run {} · bounded observation refreshed; execution may continue while closed",
                        snapshot.run.run_id
                    );
                    let text = content(&snapshot);
                    if text != self.text {
                        self.text = text;
                        self.view.lines = self
                            .text
                            .lines()
                            .map(|line| bmux_tui::prelude::Line::from(line.to_owned()))
                            .collect();
                        self.view.layout = None;
                    }
                    self.view.mark_updated();
                    self.snapshot = Some(snapshot);
                }
                Ok(Some(_)) => {
                    self.view.detail =
                        "Unsupported observation version; previous preview retained".into();
                    self.snapshot = None;
                }
                Ok(None) => {
                    self.view.detail = "No associated goal execution".into();
                    self.snapshot = None;
                }
                Err(error) => {
                    self.view.detail =
                        format!("Observation unavailable; previous preview retained: {error}");
                    self.snapshot = None;
                }
            }
        }
        if !self.pending
            && Instant::now() >= self.next_refresh
            && let Some(session) = self.session
        {
            self.pending = true;
            let run = self.run.clone();
            let completion = Arc::downgrade(&self.completion);
            host.spawn(Box::pin(async move {
                let result = async {
                    let client = BcodeClient::default_endpoint();
                    let run = match run {
                        Some(run) => run,
                        None => match client
                            .inspect_associated_workflow_run(workflow_binding_key(session), 1)
                            .await
                            .map_err(|e| e.to_string())?
                        {
                            Some(inspection) => inspection.run.run_id,
                            None => return Ok(None),
                        },
                    };
                    client
                        .workflow_run_view(run, 10)
                        .await
                        .map(Some)
                        .map_err(|e| e.to_string())
                }
                .await;
                if let Some(completion) = completion.upgrade() {
                    *completion.lock().expect("supervision result") = Some(result);
                }
            }));
        }
        PluginTuiAction::Redraw
    }
    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Event::Key(stroke) = event {
            if stroke.key == KeyCode::Escape {
                return PluginTuiAction::Close { outcome: None };
            }
            if let KeyCode::Char(key @ '1'..='9') = stroke.key
                && let Some(snapshot) = &self.snapshot
                && let Some(child) = snapshot.child_sessions.get(key as usize - '1' as usize)
                && let Ok(session_id) = SessionId::from_str(&child.session_id)
            {
                return PluginTuiAction::OpenSession { session_id };
            }
        }
        if let Some((area, layout)) = &self.view.layout {
            use bmux_tui_components::text_view::{TextViewComponent, TextViewPolicy};
            let _ = TextViewComponent::new("goal.live.output", &self.view.lines, &self.view.scroll)
                .policy(TextViewPolicy::scrollable())
                .handle_event(*area, layout, event);
        }
        PluginTuiAction::Redraw
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn supervision_reuses_bounded_results_and_labels_missing_sessions() {
        let view = crate::goal_status::tests::view();
        let text = super::content(&view);
        assert!(text.contains("No execution sessions in this bounded snapshot"));
        assert!(text.contains("not independent verification"));
        assert!(text.contains("/workflow"));
    }
}
