//! Bcode-owned root runtime message and model contracts.
//!
//! These types establish the application boundary before orchestration migrates from the existing
//! chat loop. BMUX treats messages and model state as opaque application data.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bmux_tui::event::Event;

use super::TuiError;
use super::artifact_stream::ActiveArtifactFetchCompletion;
use super::chat_loop::{ChatLoopState, DraftAutosave, TuiRuntimeSettings};
use super::effects::TuiEffectResult;
use super::history_flow;
use super::invalidation::InvalidationKey;
use super::markdown_projection_coordinator::MarkdownProjectionCompletion;
use super::session_flow::ActiveChat;

/// Typed Bcode event admitted to the root TUI runtime.
#[allow(dead_code)]
pub enum BcodeRuntimeMessage {
    /// Install root-runtime subscriptions after all application state is owned by the program.
    Bootstrap,
    /// Reliable terminal input after BMUX decoding and admission classification.
    Terminal(Event),
    /// Ordered canonical/session-view stream update.
    SessionStream(Box<history_flow::SessionStreamUpdate>),
    /// Completed artifact fetch for the active presentation generation.
    ArtifactFetchCompleted(Box<ActiveArtifactFetchCompletion>),
    /// Latest Markdown projection completion.
    MarkdownProjectionCompleted(Box<Option<MarkdownProjectionCompletion>>),
    /// Completed Bcode-owned background effect.
    EffectCompleted(Box<TuiEffectResult>),
    /// Due Bcode-owned semantic invalidations.
    Invalidations(Vec<InvalidationKey>),
    /// Draft autosave deadline.
    DraftSaveDue,
    /// Interactive-surface retry deadline.
    InteractionRetryDue,
    /// Streaming-presentation interpolation deadline.
    StreamingPresentationDue,
    /// Client telemetry flush deadline.
    TelemetryFlushDue,
}

impl BcodeRuntimeMessage {
    #[allow(dead_code)]
    fn latest_key(&self) -> Option<bmux_tui_runtime::MessageKey> {
        match self {
            Self::MarkdownProjectionCompleted(_) => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.markdown_projection",
            )),
            Self::StreamingPresentationDue => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.streaming_presentation",
            )),
            Self::TelemetryFlushDue => {
                Some(bmux_tui_runtime::MessageKey::new("bcode.telemetry_flush"))
            }
            Self::Bootstrap
            | Self::Terminal(_)
            | Self::SessionStream(_)
            | Self::ArtifactFetchCompleted(_)
            | Self::EffectCompleted(_)
            | Self::Invalidations(_)
            | Self::DraftSaveDue
            | Self::InteractionRetryDue => None,
        }
    }
}

/// Admit one Bcode message using its domain-owned reliability classification.
///
/// # Errors
///
/// Returns an admission error when the runtime is closed or keyed latest-value capacity is full.
#[allow(dead_code)]
pub async fn admit(
    handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    message: BcodeRuntimeMessage,
) -> Result<(), BcodeRuntimeAdmissionError> {
    if let Some(key) = message.latest_key() {
        handle
            .send_latest(key, message)
            .map(|_| ())
            .map_err(|error| match error {
                bmux_tui_runtime::LatestSendError::Full(_) => BcodeRuntimeAdmissionError::Full,
                bmux_tui_runtime::LatestSendError::Closed(_) => BcodeRuntimeAdmissionError::Closed,
            })
    } else {
        handle
            .send(message)
            .await
            .map_err(|_| BcodeRuntimeAdmissionError::Closed)
    }
}

/// Normalized Bcode root-runtime admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BcodeRuntimeAdmissionError {
    /// Distinct-key latest-value capacity is exhausted.
    Full,
    /// Runtime admission has closed.
    Closed,
}

///
/// Presentation caches and nested screen state remain Bcode-owned. This model deliberately does
/// not expose Bcode types through BMUX contracts.
#[allow(dead_code)]
pub struct BcodeRuntimeModel {
    /// Canonical application/session owner used by the existing chat path.
    pub chat: ActiveChat,
    /// Bcode-specific effect, plugin-surface, projection, artifact, image, and cache state.
    pub loop_state: ChatLoopState,
    /// Reloadable keymap, cadence, plugin, and launch settings.
    pub settings: TuiRuntimeSettings,
    /// Draft autosave generation and deadline state.
    pub draft_autosave: DraftAutosave,
    /// Current top-level navigation/screen state.
    pub screen: BcodeRuntimeScreen,
    /// Deferred application messages blocked by an explicit paint or navigation barrier.
    pub deferred: VecDeque<BcodeRuntimeMessage>,
    /// Current merged Bcode semantic presentation damage.
    pub invalidation: super::invalidation::UiInvalidation,
    /// Last successfully committed presentation timestamp.
    pub last_presented_at: Option<Instant>,
    /// Whether the root program should terminate after its dirty state is committed.
    pub exit_requested: bool,
}

impl BcodeRuntimeModel {
    #[allow(dead_code)]
    pub fn new(chat: ActiveChat, settings: TuiRuntimeSettings, loop_state: ChatLoopState) -> Self {
        let draft_autosave = DraftAutosave::new(
            settings.launch_working_directory().to_path_buf(),
            chat.app.composer().text().to_owned(),
        );
        Self {
            chat,
            loop_state,
            settings,
            draft_autosave,
            screen: BcodeRuntimeScreen::Chat,
            deferred: VecDeque::new(),
            invalidation: super::invalidation::UiInvalidation::Full,
            last_presented_at: None,
            exit_requested: false,
        }
    }

    #[allow(dead_code)]
    pub fn abort_all_effects(&mut self) {
        self.loop_state.abort_all_effects();
    }

    /// Mark the currently accumulated semantic damage as successfully presented.
    #[allow(dead_code)]
    pub fn presentation_committed(&mut self, at: Instant) {
        self.invalidation = super::invalidation::UiInvalidation::None;
        self.last_presented_at = Some(at);
        self.loop_state.mark_presentation_committed();
        if self
            .loop_state
            .apply_deferred_session_stream_updates(&mut self.chat)
        {
            self.invalidation = super::invalidation::UiInvalidation::Structural;
        }
    }
}

/// Synchronous root presenter preserving Bcode frame, hit-map, cursor, and image commit ordering.
#[allow(dead_code)]
pub struct BcodeRuntimePresenter<'a, 'b, W> {
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
}

impl<'a, 'b, W> BcodeRuntimePresenter<'a, 'b, W> {
    /// Create a presenter around the caller-owned terminal.
    #[allow(dead_code)]
    #[must_use]
    pub const fn new(terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>) -> Self {
        Self { terminal }
    }
}

impl<W: std::io::Write> bmux_tui_runtime::Presenter<BcodeRuntimeModel>
    for BcodeRuntimePresenter<'_, '_, W>
{
    type Error = TuiError;

    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(bmux_tui::geometry::Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }

    fn present(
        &mut self,
        program: &mut BcodeRuntimeModel,
    ) -> Result<bmux_tui_runtime::PresentReport, Self::Error> {
        let started = Instant::now();
        let frame_interval = program.settings.bmux_runtime_config().frame_interval;
        super::chat_loop::draw_chat_frame(
            self.terminal,
            &mut program.chat,
            &mut program.loop_state,
            Duration::ZERO,
            frame_interval,
        )?;
        program.presentation_committed(started);
        Ok(bmux_tui_runtime::PresentReport::default())
    }
}

impl bmux_tui_runtime::Program for BcodeRuntimeModel {
    type Message = BcodeRuntimeMessage;
    type Error = std::convert::Infallible;

    fn update(
        &mut self,
        event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
    ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
        let damage = match event {
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Bootstrap) => {
                let (_replacement_sender, replacement_receiver) = tokio::sync::mpsc::channel(1);
                let mut session_stream =
                    std::mem::replace(&mut self.chat.event_receiver, replacement_receiver);
                return Ok(bmux_tui_runtime::Update::redraw().with_subscription(
                    bmux_tui_runtime::Subscription::new(
                        bmux_tui_runtime::SubscriptionKey::new("bcode.session_stream"),
                        move |sender| async move {
                            while let Some(update) = session_stream.recv().await {
                                if sender
                                    .send(BcodeRuntimeMessage::SessionStream(Box::new(update)))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        },
                    ),
                ));
            }
            bmux_tui_runtime::RuntimeEvent::Terminal(Event::Resize(_)) => {
                super::invalidation::UiInvalidation::Full
            }
            bmux_tui_runtime::RuntimeEvent::Terminal(_)
            | bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Terminal(_)) => {
                super::invalidation::UiInvalidation::Structural
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Invalidations(keys)) => {
                self.chat.app.handle_invalidations(&keys, Instant::now())
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::StreamingPresentationDue,
            ) => {
                if self.chat.app.advance_streaming_presentation(Instant::now()) {
                    super::invalidation::UiInvalidation::Paint
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(update)) => {
                if self
                    .loop_state
                    .apply_session_stream_update(&mut self.chat, *update)
                {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::ArtifactFetchCompleted(completion),
            ) => {
                if self
                    .loop_state
                    .apply_artifact_completion(&self.chat, *completion)
                {
                    super::invalidation::UiInvalidation::Items
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::MarkdownProjectionCompleted(completion),
            ) => {
                if (*completion).is_some_and(|completion| {
                    self.loop_state
                        .apply_markdown_projection_completion(&mut self.chat, completion)
                }) {
                    super::invalidation::UiInvalidation::Items
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                message @ (BcodeRuntimeMessage::EffectCompleted(_)
                | BcodeRuntimeMessage::DraftSaveDue
                | BcodeRuntimeMessage::InteractionRetryDue
                | BcodeRuntimeMessage::TelemetryFlushDue),
            ) => {
                self.deferred.push_back(message);
                super::invalidation::UiInvalidation::Paint
            }
            bmux_tui_runtime::RuntimeEvent::Timer(_) => super::invalidation::UiInvalidation::Paint,
        };
        self.invalidation = self.invalidation.merge(damage);
        let mut update = match self.invalidation {
            super::invalidation::UiInvalidation::None => bmux_tui_runtime::Update::none(),
            super::invalidation::UiInvalidation::Full => bmux_tui_runtime::Update::reset(),
            super::invalidation::UiInvalidation::Paint
            | super::invalidation::UiInvalidation::Items
            | super::invalidation::UiInvalidation::Structural => bmux_tui_runtime::Update::redraw(),
        };
        if self.exit_requested || self.chat.app.should_exit() {
            update = update.merge(bmux_tui_runtime::Update::exit());
        }
        Ok(update)
    }
}

/// Record one live root-runtime statistics snapshot into Bcode telemetry.
pub fn record_runtime_stats(model: &mut BcodeRuntimeModel, stats: &bmux_tui_runtime::RuntimeStats) {
    model.loop_state.record_runtime_stats(stats);
    model.loop_state.flush_telemetry_if_due(Instant::now());
}

/// Consume a successful root-runtime output after recording its final neutral statistics.
#[allow(dead_code)]
pub fn finish_runtime<P>(
    mut output: bmux_tui_runtime::RuntimeOutput<BcodeRuntimeModel, P>,
) -> BcodeRuntimeModel {
    record_runtime_stats(&mut output.program, &output.stats);
    output.program.abort_all_effects();
    output.program
}

/// Run a constructed root runtime, map failures, record final statistics, and stop owned work.
#[allow(dead_code)]
pub async fn run<W: std::io::Write>(
    runtime: bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'_, '_, W>>,
) -> Result<BcodeRuntimeModel, TuiError> {
    match Box::pin(runtime.run()).await {
        Ok(output) => Ok(finish_runtime(output)),
        Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => match error {},
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, mut output }) => {
            record_runtime_stats(&mut output.program, &output.stats);
            output.program.abort_all_effects();
            Err(error)
        }
    }
}

/// Construct the root runtime and its bounded admission handle.
#[allow(dead_code)]
pub fn runtime<'a, 'b, W: std::io::Write>(
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
    model: BcodeRuntimeModel,
) -> (
    bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'a, 'b, W>>,
    bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) {
    let config = model.settings.bmux_runtime_config();
    let (runtime, handle) =
        bmux_tui_runtime::Runtime::new(model, BcodeRuntimePresenter::new(terminal), config);
    assert!(
        handle.try_send(BcodeRuntimeMessage::Bootstrap).is_ok(),
        "new root runtime accepts bootstrap message"
    );
    (runtime, handle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum BcodeRuntimeScreen {
    /// Normal chat/session presentation.
    #[default]
    Chat,
    /// Plugin-contributed terminal surface.
    PluginSurface,
    /// Session picker or transcript-search surface.
    SessionPicker,
    /// Onboarding/setup surface.
    Onboarding,
}

#[cfg(test)]
mod tests {
    use super::{BcodeRuntimeAdmissionError, BcodeRuntimeMessage, admit};
    use std::convert::Infallible;

    fn assert_runtime_message_is_send<T: Send + 'static>() {}

    #[test]
    fn root_message_contract_is_runtime_admissible() {
        assert_runtime_message_is_send::<BcodeRuntimeMessage>();
    }

    #[test]
    fn root_runtime_and_presenter_types_compose() {
        fn assert_runtime<P, R>()
        where
            P: bmux_tui_runtime::Program<Message = BcodeRuntimeMessage>,
            R: bmux_tui_runtime::Presenter<P>,
        {
        }

        assert_runtime::<
            super::BcodeRuntimeModel,
            super::BcodeRuntimePresenter<'static, 'static, Vec<u8>>,
        >();
    }

    #[derive(Default)]
    struct AdmissionProgram {
        received: usize,
    }

    impl bmux_tui_runtime::Program for AdmissionProgram {
        type Message = BcodeRuntimeMessage;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            if matches!(event, bmux_tui_runtime::RuntimeEvent::Message(_)) {
                self.received += 1;
            }
            Ok(if self.received == 2 {
                bmux_tui_runtime::Update::exit()
            } else {
                bmux_tui_runtime::Update::none()
            })
        }
    }

    #[tokio::test]
    async fn domain_owned_admission_separates_reliable_and_latest_messages() {
        let config = bmux_tui_runtime::RuntimeConfig {
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            AdmissionProgram::default(),
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        admit(&handle, BcodeRuntimeMessage::DraftSaveDue)
            .await
            .expect("reliable message admitted");
        admit(&handle, BcodeRuntimeMessage::StreamingPresentationDue)
            .await
            .expect("latest message admitted");
        let output = runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("runtime succeeds"));
        assert_eq!(output.program.received, 2);
        assert_eq!(output.stats.reliable_processed, 1);
        assert_eq!(output.stats.latest_processed, 1);
        assert_ne!(
            BcodeRuntimeAdmissionError::Full,
            BcodeRuntimeAdmissionError::Closed
        );
    }
}
