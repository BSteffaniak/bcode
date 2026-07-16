//! Host adapter for native plugin-owned TUI surfaces.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_plugin_sdk::tui::{
    PluginSessionEvent, PluginSessionEventReplay, PluginSessionEventSubscription,
    PluginSessionEventSubscriptionRequest, PluginTask, PluginTuiAction, PluginTuiHost,
    PluginTuiHostError, PluginTuiSurface,
};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::terminal_events::TuiInput;
use super::{TuiError, helpers};

const DEFAULT_PLUGIN_SESSION_EVENT_BUFFER: usize = 256;
const MAX_PLUGIN_SESSION_EVENT_BUFFER: usize = 4096;

/// Host services for plugin-owned TUI surfaces running inside Bcode's TUI.
#[derive(Debug, Clone)]
struct BcodePluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw_sender: mpsc::UnboundedSender<()>,
    client: BcodeClient,
}

impl BcodePluginTuiHost {
    /// Create a plugin TUI host from the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    fn current(redraw_sender: mpsc::UnboundedSender<()>, client: BcodeClient) -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
            redraw_sender,
            client,
        }
    }
}

impl PluginTuiHost for BcodePluginTuiHost {
    fn spawn(&self, task: PluginTask) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn(async move {
            task.await;
            let _ = redraw_sender.send(());
        }));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn_blocking(move || {
            task();
            let _ = redraw_sender.send(());
        }));
    }

    fn request_redraw(&self) {
        let _ = self.redraw_sender.send(());
    }

    fn subscribe_session_events(
        &self,
        request: PluginSessionEventSubscriptionRequest,
    ) -> Result<PluginSessionEventSubscription, PluginTuiHostError> {
        let buffer = request
            .buffer
            .clamp(1, MAX_PLUGIN_SESSION_EVENT_BUFFER)
            .max(DEFAULT_PLUGIN_SESSION_EVENT_BUFFER.min(MAX_PLUGIN_SESSION_EVENT_BUFFER));
        let (sender, receiver) = mpsc::channel(buffer);
        let client = self.client.clone();
        let redraw_sender = self.redraw_sender.clone();
        drop(self.handle.spawn(async move {
            stream_plugin_session_events(client, request, sender, redraw_sender).await;
        }));
        Ok(PluginSessionEventSubscription { receiver })
    }
}

async fn stream_plugin_session_events(
    client: BcodeClient,
    request: PluginSessionEventSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionEvent>,
    redraw_sender: mpsc::UnboundedSender<()>,
) {
    if let Err(error) =
        stream_plugin_session_events_inner(client, request, sender.clone(), redraw_sender.clone())
            .await
    {
        let _ = sender
            .send(PluginSessionEvent::Disconnected {
                message: error.to_string(),
            })
            .await;
        let _ = redraw_sender.send(());
    }
}

async fn stream_plugin_session_events_inner(
    client: BcodeClient,
    request: PluginSessionEventSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionEvent>,
    redraw_sender: mpsc::UnboundedSender<()>,
) -> Result<(), bcode_client::ClientError> {
    let session_id = request.session_id;
    let mut connection = client.connect("bcode-plugin-tui-session-events").await?;
    let attached = match request.replay {
        PluginSessionEventReplay::None => {
            connection
                .attach_session_recent_with_input_history(session_id, 0)
                .await?
        }
        PluginSessionEventReplay::Recent { limit } => {
            connection
                .attach_session_recent_with_input_history(session_id, limit)
                .await?
        }
        PluginSessionEventReplay::ProjectionWindow { request } => {
            connection
                .attach_session_projection_window_with_input_history(session_id, request)
                .await?
        }
    };
    if sender
        .send(PluginSessionEvent::Attached {
            session: attached.session,
            history: attached.history,
        })
        .await
        .is_err()
    {
        return Ok(());
    }
    let _ = redraw_sender.send(());

    loop {
        let event = connection.recv_event().await?;
        let plugin_event = match event {
            BcodeEvent::Session(event) if event.session_id == session_id => {
                Some(PluginSessionEvent::Session(event))
            }
            BcodeEvent::SessionLive(event) if event.session_id == session_id => {
                Some(PluginSessionEvent::SessionLive(event))
            }
            BcodeEvent::Session(_)
            | BcodeEvent::SessionLive(_)
            | BcodeEvent::RuntimeWork(_)
            | BcodeEvent::SessionViewResyncRequired { .. }
            | BcodeEvent::SessionCatalogUpdated { .. } => None,
        };
        let Some(plugin_event) = plugin_event else {
            continue;
        };
        if sender.send(plugin_event).await.is_err() {
            return Ok(());
        }
        let _ = redraw_sender.send(());
    }
}

/// Run one plugin-owned native TUI surface with a fresh terminal input stream and return its close outcome.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    surface: &mut dyn PluginTuiSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let mut input = TuiInput::start();
    run_plugin_surface_with_input(terminal, &mut input, surface).await
}

/// Run one plugin-owned native TUI surface with the caller-owned terminal input stream.
///
/// Use this when a plugin surface is nested inside the main TUI runtime so there is only one
/// terminal event reader.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface_with_input<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    surface: &mut dyn PluginTuiSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let client = BcodeClient::default_endpoint();
    run_plugin_surface_with_input_and_client(terminal, input, surface, client).await
}

/// Run one plugin-owned native TUI surface with an explicit Bcode client.
///
/// # Errors
///
/// Returns an error when terminal I/O or terminal input fails.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface_with_input_and_client<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    surface: &mut dyn PluginTuiSurface,
    client: BcodeClient,
) -> Result<Option<serde_json::Value>, TuiError> {
    let (redraw_sender, mut redraw_receiver) = mpsc::unbounded_channel();
    let host = BcodePluginTuiHost::current(redraw_sender, client);
    let mut needs_redraw = true;
    let mut close_outcome = None;
    let mut should_exit = false;

    while !should_exit {
        if helpers::resize_from_terminal(terminal)? {
            needs_redraw = true;
        }
        if surface.poll(&host).requests_redraw() {
            needs_redraw = true;
        }
        if surface.drain_effects(&host).await.requests_redraw() {
            needs_redraw = true;
        }
        if needs_redraw {
            terminal.draw(|frame| {
                let area = frame.area();
                surface.render(area, frame);
            })?;
            needs_redraw = false;
        }

        tokio::select! {
            event = input.recv() => {
                let Some(event) = event? else {
                    continue;
                };
                if handle_host_event(terminal, &event) {
                    needs_redraw = true;
                }
                match surface.handle_event(&event, &host) {
                    PluginTuiAction::None => {}
                    PluginTuiAction::Redraw => needs_redraw = true,
                    PluginTuiAction::Close { outcome } => {
                        close_outcome = outcome;
                        should_exit = true;
                    }
                    PluginTuiAction::OpenSurface { .. } => {
                        needs_redraw = true;
                    }
                    PluginTuiAction::RunCommand { command } => {
                        close_outcome = Some(serde_json::json!({ "run_command": command }));
                        should_exit = true;
                    }
                }
            }
            redraw = redraw_receiver.recv() => {
                if redraw.is_some() {
                    needs_redraw = true;
                }
            }
        }
    }

    Ok(close_outcome)
}

fn handle_host_event<W: Write>(terminal: &mut Terminal<&mut W>, event: &Event) -> bool {
    match event {
        Event::Resize(size) => {
            terminal.resize(Rect::new(0, 0, size.width, size.height));
            true
        }
        Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick => true,
        Event::Key(_) | Event::Mouse(_) | Event::Paste(_) | Event::User(_) => false,
    }
}
