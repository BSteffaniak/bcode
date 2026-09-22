//! Read-only live view of the existing run-scoped working document.
use super::{
    Arc, BcodeClient, BoxedPluginTuiSurface, ClientError, Event, FromStr, KeyCode, Mutex, PaintCx,
    PluginTuiAction, PluginTuiHost, PluginTuiSurface, PluginTuiSurfaceFactory,
    PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest, Rect, SessionId, workflow_binding_key,
};

pub const SURFACE: &str = "goal.progress.live";
pub struct Factory;
impl PluginTuiSurfaceFactory for Factory {
    fn surface_kind(&self) -> &'static str {
        SURFACE
    }
    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let session = request
                .options
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| SessionId::from_str(value).ok());
            Ok(Box::new(DocumentView {
                session,
                run_id: None,
                pending: false,
                result: Arc::default(),
                next_refresh: std::time::Instant::now(),
                view: {
                    let mut view = crate::goal_live::GenerationView::default();
                    view.title = "Goal · Progress document";
                    view.footer =
                        "Saved working notes, not proof of readiness or completion · Esc closes";
                    view
                },
                text: String::new(),
            }) as BoxedPluginTuiSurface)
        })
    }
}
type DocumentResult = Result<Option<bcode_session_models::SessionWorkingDocument>, String>;
type DocumentCompletion = Arc<Mutex<Option<(Option<String>, DocumentResult)>>>;
struct DocumentView {
    session: Option<SessionId>,
    run_id: Option<String>,
    pending: bool,
    result: DocumentCompletion,
    next_refresh: std::time::Instant,
    view: crate::goal_live::GenerationView,
    text: String,
}
impl PluginTuiSurface for DocumentView {
    fn id(&self) -> &'static str {
        SURFACE
    }
    fn title(&self) -> &'static str {
        "Progress document"
    }
    fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
        crate::goal::paint_generation(&mut self.view, area, frame);
    }
    fn poll(&mut self, host: &dyn PluginTuiHost) -> PluginTuiAction {
        let result = self.result.lock().expect("document result").take();
        if let Some((run_id, result)) = result {
            self.pending = false;
            if run_id.is_some() {
                self.run_id = run_id;
            }
            self.next_refresh = std::time::Instant::now() + std::time::Duration::from_secs(2);
            match result {
                Ok(Some(document)) => {
                    self.view.detail = format!(
                        "Saved document: {} · {}",
                        document.path,
                        crate::progress::Checklist::parse(&document.text).summary()
                    );
                    if document.text != self.text {
                        self.text = document.text;
                        self.view.mark_updated();
                        self.view.layout = None;
                        self.view.lines = self
                            .text
                            .lines()
                            .map(|line| bmux_tui::prelude::Line::from(line.to_owned()))
                            .collect();
                    }
                }
                Ok(None) => {
                    self.view.detail = "No progress document for this run".into();
                }
                Err(error) => {
                    self.view.detail =
                        format!("Document unavailable (previous preview retained): {error}");
                }
            }
        }
        if !self.pending
            && std::time::Instant::now() >= self.next_refresh
            && let Some(session_id) = self.session
        {
            self.pending = true;
            let result = Arc::clone(&self.result);
            let run_id = self.run_id.clone();
            host.spawn(Box::pin(async move {
                let client = BcodeClient::default_endpoint();
                let outcome = async {
                    let run_id = match run_id {
                        Some(run_id) => run_id,
                        None => match client
                            .inspect_associated_workflow_run(workflow_binding_key(session_id), 1)
                            .await?
                        {
                            Some(inspection) => inspection
                                .continuation
                                .map_or(inspection.run.run_id, |lineage| lineage.document_scope_id),
                            None => return Ok((None, None)),
                        },
                    };
                    let document = client
                        .session_working_document(
                            bcode_session_models::SessionWorkingDocumentRequest {
                                version: 1,
                                session_id,
                                scope_id: run_id.clone(),
                                initial_text: None,
                            },
                        )
                        .await?;
                    Ok::<_, ClientError>((Some(run_id), document))
                }
                .await;
                *result.lock().expect("document result") = Some(match outcome {
                    Ok((run_id, document)) => (run_id, Ok(document)),
                    Err(error) => (None, Err(error.to_string())),
                });
            }));
        }
        PluginTuiAction::Redraw
    }
    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if matches!(event, Event::Key(stroke) if stroke.key == KeyCode::Escape) {
            return PluginTuiAction::Close { outcome: None };
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
