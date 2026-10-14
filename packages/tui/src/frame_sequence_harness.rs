//! Deterministic transcript frame-sequence acceptance harness.

use super::app::{BmuxApp, TranscriptFrameObservation};
use super::render;
use bcode_session_models::{SessionEvent, SessionLiveEvent};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFrameSnapshot {
    pub label: String,
    pub text: String,
    pub observation: TranscriptFrameObservation,
    pub markdown_fresh_equivalent: bool,
    /// Painted cells retained with their styles so acceptance checks can detect lost highlighting.
    pub cells: Vec<bmux_tui::buffer::Cell>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFrameSequenceError {
    pub frame_index: usize,
    pub frame_label: String,
    pub message: String,
    pub previous_frame: Option<String>,
    pub frame: String,
}

impl std::fmt::Display for TranscriptFrameSequenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            formatter,
            "frame {} ({}) violated acceptance: {}",
            self.frame_index, self.frame_label, self.message
        )?;
        if let Some(previous) = &self.previous_frame {
            writeln!(formatter, "--- previous frame ---\n{previous}")?;
        }
        write!(formatter, "--- failing frame ---\n{}", self.frame)
    }
}

pub fn assert_no_forbidden_frames(
    frames: &[TranscriptFrameSnapshot],
    forbidden: impl Fn(&TranscriptFrameSnapshot) -> Option<String>,
) -> Result<(), TranscriptFrameSequenceError> {
    for (frame_index, frame) in frames.iter().enumerate() {
        if let Some(message) = forbidden(frame) {
            return Err(TranscriptFrameSequenceError {
                frame_index,
                frame_label: frame.label.clone(),
                message,
                previous_frame: frame_index
                    .checked_sub(1)
                    .and_then(|index| frames.get(index))
                    .map(|frame| frame.text.clone()),
                frame: frame.text.clone(),
            });
        }
    }
    Ok(())
}

pub enum TranscriptFrameInput {
    EphemeralPlain(String),
    Durable(Box<SessionEvent>),
    Live(Box<SessionLiveEvent>),
    PrependHistory {
        events: Vec<SessionEvent>,
        has_more: bool,
    },
    AppendHistory {
        events: Vec<SessionEvent>,
        has_more: bool,
    },
    CenterHistory {
        events: Vec<SessionEvent>,
        anchor_sequence: u64,
        has_older: bool,
        has_newer: bool,
    },
    DurableBatch(Vec<SessionEvent>),
    Resize(u16, u16),
    ScrollUp(usize),
    ScrollDown(usize),
    ScrollToBottom,
    ReasoningDisplayMode(bcode_config::TuiThinkingMode),
    ThinkingConfig(bcode_config::TuiThinkingConfig),
    AdvanceStreaming(std::time::Duration),
    /// Advance only scroll-animation time; ordinary observations share one frozen instant.
    AdvanceScrollAnimation(std::time::Duration),
    AssertNoPendingStreaming,
    Observe,
}

fn live_input(event: SessionLiveEvent) -> TranscriptFrameInput {
    TranscriptFrameInput::Live(Box::new(event))
}

fn durable_input(event: SessionEvent) -> TranscriptFrameInput {
    TranscriptFrameInput::Durable(Box::new(event))
}

pub struct TranscriptFrameStep {
    pub label: &'static str,
    pub input: TranscriptFrameInput,
}

/// Borrows the application under test; captured frames own their observations so
/// callers can release plugin/runtime resources immediately after the sequence.
pub struct TranscriptFrameSequence<'a> {
    app: &'a mut BmuxApp,
    width: u16,
    height: u16,
    frames: Vec<TranscriptFrameSnapshot>,
}

impl<'a> TranscriptFrameSequence<'a> {
    pub const fn new(app: &'a mut BmuxApp, width: u16, height: u16) -> Self {
        Self {
            app,
            width,
            height,
            frames: Vec::new(),
        }
    }

    fn with_plugin_host(self, host: std::sync::Arc<bcode_plugin::PluginHost>) -> Self {
        self.app.set_plugin_host(host);
        self
    }

    pub fn run(
        mut self,
        steps: impl IntoIterator<Item = TranscriptFrameStep>,
    ) -> Vec<TranscriptFrameSnapshot> {
        // Rendering samples animation time more than once. Keep both samples and consecutive
        // observations stable regardless of wall-clock delays or parallel suite scheduling.
        self.app.freeze_transcript_scroll_animation_for_test();
        for step in steps {
            match step.input {
                TranscriptFrameInput::EphemeralPlain(text) => {
                    self.app.push_ephemeral_system_plain(text);
                }
                TranscriptFrameInput::Durable(event) => self.app.absorb_session_event(&event),
                TranscriptFrameInput::Live(event) => self.app.absorb_session_live_event(&event),
                TranscriptFrameInput::PrependHistory { events, has_more } => {
                    self.app.prepend_older_history(&events, has_more);
                }
                TranscriptFrameInput::AppendHistory { events, has_more } => {
                    self.app.append_newer_history(&events, has_more);
                }
                TranscriptFrameInput::CenterHistory {
                    events,
                    anchor_sequence,
                    has_older,
                    has_newer,
                } => {
                    self.app.replace_transcript_window(
                        &events,
                        has_older,
                        has_newer,
                        anchor_sequence,
                    );
                    self.app
                        .request_transcript_top_anchor_sequence(anchor_sequence);
                }
                TranscriptFrameInput::DurableBatch(events) => {
                    for event in events {
                        self.app.absorb_session_event(&event);
                    }
                }
                TranscriptFrameInput::Resize(width, height) => {
                    self.width = width;
                    self.height = height;
                }
                TranscriptFrameInput::ScrollDown(rows) => {
                    self.app.scroll_transcript_down(rows);
                }
                TranscriptFrameInput::ScrollToBottom => {
                    self.app.scroll_transcript_to_bottom();
                }
                TranscriptFrameInput::ScrollUp(rows) => {
                    let _ = self.app.scroll_transcript_up(rows);
                }
                TranscriptFrameInput::AdvanceStreaming(after_deadline) => {
                    let deadline = self
                        .app
                        .next_streaming_presentation_deadline(std::time::Instant::now())
                        .expect("frame step requires pending stream presentation");
                    assert!(
                        self.app
                            .advance_streaming_presentation(deadline + after_deadline)
                    );
                }
                TranscriptFrameInput::AssertNoPendingStreaming => {
                    assert!(
                        self.app
                            .next_streaming_presentation_deadline(std::time::Instant::now())
                            .is_none(),
                        "frame step requires streaming presentation to be stopped"
                    );
                }
                TranscriptFrameInput::ReasoningDisplayMode(mode) => {
                    self.app.set_reasoning_display_mode(mode);
                }
                TranscriptFrameInput::ThinkingConfig(config) => {
                    self.app.apply_thinking_config(config);
                }
                TranscriptFrameInput::AdvanceScrollAnimation(duration) => {
                    self.app
                        .advance_transcript_scroll_animation_for_test(duration);
                }
                TranscriptFrameInput::Observe => {}
            }
            let frame = self.capture(step.label);
            self.frames.push(frame);
        }
        self.frames
    }

    fn capture(&mut self, label: &str) -> TranscriptFrameSnapshot {
        // The production chat loop accepts background completions between frames. This isolated
        // application harness has no loop/worker, so synchronously install the same exact
        // generation before capturing each deterministic accepted frame.
        let markdown_items = self
            .app
            .transcript()
            .iter()
            .filter(|item| item.text_format() == bcode_session_view_models::TextFormat::Markdown)
            .cloned()
            .collect::<Vec<_>>();
        for item in &markdown_items {
            let options = render::markdown_render_options(self.app, item, self.width);
            self.app.transcript_markdown_cache().project(item, options);
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, self.width, self.height));
        let mut frame = Frame::new(&mut buffer);
        render::render(self.app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        let text = (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        let markdown_fresh_equivalent = self
            .app
            .transcript()
            .iter()
            .filter(|item| item.text_format() == bcode_session_view_models::TextFormat::Markdown)
            .all(|item| {
                let options = render::markdown_render_options(self.app, item, self.width);
                self.app
                    .transcript_markdown_cache()
                    .get(item.id().get(), item.revision(), &options)
                    .is_some_and(|accepted| {
                        *accepted == bcode_markdown_render::render_markdown(item.text(), &options)
                    })
            });
        TranscriptFrameSnapshot {
            label: label.to_owned(),
            text,
            observation: self.app.frame_observation(),
            markdown_fresh_equivalent,
            cells: buffer.cells().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ClientId, RuntimeWorkKind, RuntimeWorkStatus, SessionEventKind, SessionId,
        SessionTokenUsage, TextStreamOperation, TextStreamUpdate, ToolInvocationResult, WorkId,
    };
    use std::sync::Arc;

    #[test]
    fn newer_page_updating_existing_item_does_not_spend_unearned_movement() {
        let session_id = SessionId::new();
        let message = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let centered = (10..18).map(message).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &centered, &[], true);
        let before = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "centered",
                input: TranscriptFrameInput::CenterHistory {
                    events: centered,
                    anchor_sequence: 14,
                    has_older: false,
                    has_newer: true,
                },
            },
            TranscriptFrameStep {
                label: "request more",
                input: TranscriptFrameInput::ScrollDown(200),
            },
        ]);
        assert!(app.should_load_newer_history());
        let previous_items = before[1].observation.semantic_items.len();
        let previous_anchor = before[1].observation.anchor.clone();
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "non-presenting newer",
                input: TranscriptFrameInput::AppendHistory {
                    events: vec![durable(
                        session_id,
                        18,
                        SessionEventKind::ClientAttached {
                            client_id: ClientId::new(),
                        },
                    )],
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "repeat",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "presenting newer",
                input: TranscriptFrameInput::AppendHistory {
                    events: vec![message(19)],
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        drop(app);
        assert_eq!(frames[0].observation.semantic_items.len(), previous_items);
        assert_eq!(
            frames[0].observation.viewport_top,
            before[1].observation.viewport_top
        );
        assert_eq!(frames[0].observation, frames[1].observation);
        assert_eq!(frames[0].observation.anchor, previous_anchor);
        assert_eq!(
            frames[2].observation.semantic_items.len(),
            previous_items + 1
        );
        assert_eq!(frames[2].observation, frames[3].observation);
    }

    #[test]
    fn centered_newer_boundary_waits_for_actual_beyond_window_input() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let centered = (10..18).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &centered, &[], true);
        let first = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "centered",
            input: TranscriptFrameInput::CenterHistory {
                events: centered,
                anchor_sequence: 14,
                has_older: false,
                has_newer: true,
            },
        }]);
        let total_rows = first[0].observation.terminal_rows.len();
        let viewport_height = usize::from(app.transcript_viewport_height_for_test());
        let boundary = total_rows.saturating_sub(viewport_height);
        let remaining = boundary
            .saturating_sub(first[0].observation.viewport_top)
            .saturating_sub(2);
        assert!(
            remaining > 0,
            "the centered window must have resident rows below"
        );
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "near newer boundary",
                input: TranscriptFrameInput::ScrollDown(remaining),
            },
            TranscriptFrameStep {
                label: "settled near boundary",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(
            frames[0].observation.viewport_top,
            boundary.saturating_sub(2)
        );
        assert_eq!(frames[0].observation, frames[1].observation);
        assert!(!app.should_load_newer_history());
        // The latest-bar height can change near the resident bottom; use an
        // accepted frame to resolve the geometry before requesting another row.
        let beyond = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "finish resident rows",
                input: TranscriptFrameInput::ScrollDown(1),
            },
            TranscriptFrameStep {
                label: "one row beyond",
                input: TranscriptFrameInput::ScrollDown(1),
            },
            TranscriptFrameStep {
                label: "waiting for newer page",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(
            beyond[0].observation.viewport_top,
            boundary.saturating_sub(1)
        );
        assert!(app.should_load_newer_history());
        assert_eq!(beyond[1].observation, beyond[2].observation);
        drop(app);
    }

    #[test]
    fn accepted_newer_page_counts_presenting_rows_not_reflowed_existing_rows() {
        let session_id = SessionId::new();
        let event = |sequence, text: String| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage { text },
            )
        };
        let centered = (10..18)
            .map(|sequence| {
                event(
                    sequence,
                    format!("message {sequence}: {}", "long line ".repeat(12)),
                )
            })
            .collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &centered, &[], true);
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "centered",
                input: TranscriptFrameInput::CenterHistory {
                    events: centered,
                    anchor_sequence: 14,
                    has_older: false,
                    has_newer: true,
                },
            },
            TranscriptFrameStep {
                label: "newer boundary",
                input: TranscriptFrameInput::ScrollDown(200),
            },
        ]);
        let before = &frames[1].observation;
        assert!(app.should_load_newer_history());
        let page = vec![event(18, "newer page".to_owned())];
        let after = TranscriptFrameSequence::new(&mut app, 24, 12).run([
            TranscriptFrameStep {
                label: "accepted at narrower width",
                input: TranscriptFrameInput::AppendHistory {
                    events: page,
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "repeat",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        let accepted = &after[0].observation;
        let anchor = before.anchor.as_ref().expect("reader anchor before page");
        let old_source = before
            .terminal_rows
            .get(before.viewport_top + anchor.viewport_row_offset)
            .and_then(|(source, _)| source.as_ref())
            .expect("source row before page");
        let corresponding_row = app
            .transcript()
            .iter()
            .enumerate()
            .find(|(_, item)| item.source_view_item_id() == Some(old_source))
            .and_then(|(index, _)| {
                app.transcript_layout()
                    .entry_start_row(
                        super::super::transcript_layout::VisibleTranscriptSource::Transcript,
                        index,
                    )
                    .zip(app.transcript_layout().source_row(
                        index,
                        anchor.source_position.expect("mapped source position"),
                    ))
            })
            .map(|(start, row)| start + row)
            .expect("reader source survives reflow");
        let movement = accepted.viewport_top.saturating_sub(corresponding_row);
        let pending_request = app.should_load_newer_history();
        assert_eq!(accepted, &after[1].observation);
        let page_rows = app
            .transcript()
            .iter()
            .enumerate()
            .filter(|(_, item)| item.event_sequence() == Some(18))
            .filter_map(|(index, _)| {
                app.transcript_layout().entry_row_count(
                    super::super::transcript_layout::VisibleTranscriptSource::Transcript,
                    index,
                )
            })
            .sum::<usize>();
        drop(app);
        assert!(page_rows > 0);
        assert!(!pending_request);
        assert!(
            movement <= page_rows + anchor.viewport_row_offset,
            "reader moved {movement} rows; page {page_rows} rows"
        );
    }

    #[test]
    fn centered_newer_page_preserves_reader_and_reaches_tail() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(40)], &[], true);
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "centered",
                input: TranscriptFrameInput::CenterHistory {
                    events: (10..22).map(event).collect(),
                    anchor_sequence: 17,
                    has_older: true,
                    has_newer: true,
                },
            },
            TranscriptFrameStep {
                label: "toward newer edge",
                input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "page of newer history",
                input: TranscriptFrameInput::AppendHistory {
                    events: (22..40).map(event).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "accepted again",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        let before = &frames[1].observation;
        let after = &frames[2].observation;
        assert!(after.viewport_top >= before.viewport_top);
        assert_eq!(after, &frames[3].observation);
        assert!(after.terminal_rows.iter().any(|(id, _)| id.is_some()));
        let tail = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "return to live tail",
                input: TranscriptFrameInput::ScrollToBottom,
            },
            TranscriptFrameStep {
                label: "new live event after history navigation",
                input: TranscriptFrameInput::Durable(Box::new(event(40))),
            },
            TranscriptFrameStep {
                label: "live tail settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(tail[1].observation, tail[2].observation);
        assert!(
            tail[1].observation.semantic_items.len() > tail[0].observation.semantic_items.len()
        );
        assert!(tail[1].observation.viewport_top >= tail[0].observation.viewport_top);
    }

    #[test]
    fn scroll_animation_moves_only_when_frame_time_advances() {
        let session_id = SessionId::new();
        let events = (1..31)
            .map(|sequence| {
                durable(
                    session_id,
                    sequence,
                    SessionEventKind::AssistantMessage {
                        text: format!("message {sequence}"),
                    },
                )
            })
            .collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
        TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "reader at top",
            input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
        }]);
        assert!(app.transition_transcript_to_bottom());
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "transition started",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "same animation instant",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "transition completed",
                input: TranscriptFrameInput::AdvanceScrollAnimation(
                    std::time::Duration::from_secs(1),
                ),
            },
            TranscriptFrameStep {
                // Completion removes the latest bar. The next preparation accepts the newly
                // available body row before subsequent observations have stable geometry.
                label: "tail geometry settled after latest bar removal",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "settled tail observed again",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(frames[0].observation, frames[1].observation);
        assert!(frames[2].observation.viewport_top > frames[1].observation.viewport_top);
        assert_eq!(frames[2].observation.scroll_mode, "bottom_follow");
        assert_eq!(frames[3].observation.scroll_mode, "bottom_follow");
        assert_eq!(frames[3].observation, frames[4].observation);
        assert_eq!(app.scroll_offset(), 0);
    }

    #[test]
    fn non_presenting_older_page_does_not_spend_boundary_movement() {
        let session_id = SessionId::new();
        let message = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let recent = (30..42).map(message).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        let initial = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "resumed tail",
            input: TranscriptFrameInput::Observe,
        }]);
        let top = initial[0].observation.viewport_top;
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "reach boundary",
                input: TranscriptFrameInput::ScrollUp(top),
            },
            TranscriptFrameStep {
                label: "request rows",
                input: TranscriptFrameInput::ScrollUp(3),
            },
            TranscriptFrameStep {
                label: "only non-presenting events arrive",
                input: TranscriptFrameInput::PrependHistory {
                    events: (20..30)
                        .map(|sequence| {
                            durable(
                                session_id,
                                sequence,
                                SessionEventKind::ClientAttached {
                                    client_id: ClientId::new(),
                                },
                            )
                        })
                        .collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "repeat",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "next presenting page",
                input: TranscriptFrameInput::PrependHistory {
                    events: (1..20).map(message).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(frames[1].observation.anchor, frames[2].observation.anchor);
        assert_eq!(frames[2].observation, frames[3].observation);
        let reader = frames[2]
            .observation
            .anchor
            .as_ref()
            .expect("reader at boundary");
        let source = frames[2]
            .observation
            .terminal_rows
            .get(frames[2].observation.viewport_top + reader.viewport_row_offset)
            .and_then(|(source, _)| source.as_ref())
            .expect("reader source");
        let corresponding_row = frames[4]
            .observation
            .terminal_rows
            .iter()
            .position(|(id, row)| id.as_ref() == Some(source) && *row == reader.row_in_item)
            .expect("reader survives next page");
        assert!(corresponding_row >= frames[4].observation.viewport_top);
        assert!(corresponding_row - frames[4].observation.viewport_top <= 4);
        assert_eq!(frames[4].observation, frames[5].observation);
        assert!(frames[4].observation.viewport_top > frames[2].observation.viewport_top);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn newer_page_eviction_keeps_source_row_and_reloadable_older_cursor() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
        };
        let initial = (1..=1001).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &initial, &[], true);
        let at_tail = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "start at tail",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "detach near tail",
                input: TranscriptFrameInput::ScrollUp(2),
            },
        ]);
        let anchor = at_tail[1].observation.anchor.as_ref().expect("reader");
        let source = at_tail[1].observation.terminal_rows
            [at_tail[1].observation.viewport_top + anchor.viewport_row_offset]
            .0
            .clone()
            .expect("source");
        let row = anchor.row_in_item;
        let offset = anchor.viewport_row_offset;
        let page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "newer page crosses trim threshold",
                input: TranscriptFrameInput::AppendHistory {
                    events: (1002..=1100).map(event).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "settled after prefix eviction",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert!(app.resident_transcript_oldest_sequence().expect("oldest") > 1);
        assert!(app.resident_transcript_event_count() <= 1024);
        assert!(app.older_history_cursor().is_some());
        assert_eq!(page[0].observation, page[1].observation);
        let reader = &page[0].observation;
        let source_row = reader
            .terminal_rows
            .iter()
            .position(|(id, index)| id.as_ref() == Some(&source) && *index == row)
            .expect("reader survives trim");
        assert_eq!(source_row.saturating_sub(reader.viewport_top), offset);
        let cursor = app.older_history_cursor().expect("evicted older cursor");
        let reloaded = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "reload evicted older prefix",
                input: TranscriptFrameInput::PrependHistory {
                    events: (1..=cursor.sequence).map(event).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "settled older reload",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(app.resident_transcript_oldest_sequence(), Some(1));
        assert_eq!(reloaded[0].observation, reloaded[1].observation);
        let reload_anchor = reloaded[0]
            .observation
            .anchor
            .as_ref()
            .expect("reader after reload");
        let reload_source = reloaded[0].observation.terminal_rows
            [reloaded[0].observation.viewport_top + reload_anchor.viewport_row_offset]
            .0
            .clone()
            .expect("source after reload");
        let resized = TranscriptFrameSequence::new(&mut app, 52, 12).run([
            TranscriptFrameStep {
                label: "narrower after older prefix reload",
                input: TranscriptFrameInput::Resize(52, 12),
            },
            TranscriptFrameStep {
                label: "settled narrower reload",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(resized[0].observation, resized[1].observation);
        assert!(
            resized[0]
                .observation
                .terminal_rows
                .iter()
                .any(|(id, _)| id.as_ref() == Some(&reload_source))
        );
        drop(app);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn evicted_newer_boundary_reveals_only_loaded_rows_once() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
        };
        let recent = (500..=1500).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "initial frame",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "older boundary",
                input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "evict newer suffix",
                input: TranscriptFrameInput::PrependHistory {
                    events: (450..500).map(event).collect(),
                    has_more: true,
                },
            },
        ]);
        let cursor = app.newer_history_cursor().expect("evicted suffix cursor");
        let boundary = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "move to newer boundary",
                input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "queue additional newer rows",
                input: TranscriptFrameInput::ScrollDown(3),
            },
        ]);
        assert!(app.should_load_newer_history());
        let before = &boundary[1].observation;
        let page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "newer page",
                input: TranscriptFrameInput::AppendHistory {
                    events: (cursor.sequence..cursor.sequence + 8).map(event).collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "newer page settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        let after = &page[0].observation;
        assert_eq!(after, &page[1].observation);
        assert!(after.viewport_top >= before.viewport_top);
        assert!(after.viewport_top - before.viewport_top <= 8 * 4);
        let previous = before.anchor.as_ref().expect("source-backed reader");
        let source = before.terminal_rows[before.viewport_top + previous.viewport_row_offset]
            .0
            .as_ref()
            .expect("reader source");
        let corresponding_row = after
            .terminal_rows
            .iter()
            .position(|(id, row)| id.as_ref() == Some(source) && *row == previous.row_in_item)
            .expect("reader survives page");
        let movement = after
            .viewport_top
            .saturating_sub(corresponding_row.saturating_sub(previous.viewport_row_offset));
        assert!(
            movement > 0 && movement <= 8 * 4,
            "newer page moved {movement} rows"
        );
        assert_eq!(
            app.newer_history_cursor().map(|cursor| cursor.sequence),
            Some(cursor.sequence + 8)
        );
        assert!(app.should_load_newer_history());
        assert_eq!(
            app.newer_history_cursor().map(|cursor| cursor.sequence),
            Some(cursor.sequence + 8)
        );
        let older_before = app.older_history_cursor();
        let next_cursor = app.newer_history_cursor().expect("next newer page");
        let next_page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "second newer page",
                input: TranscriptFrameInput::AppendHistory {
                    events: (next_cursor.sequence..next_cursor.sequence + 8)
                        .map(event)
                        .collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "second newer page settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(next_page[0].observation, next_page[1].observation);
        assert!(next_page[0].observation.viewport_top >= after.viewport_top);
        assert!(next_page[0].observation.viewport_top - after.viewport_top <= 8 * 4);
        assert_eq!(app.older_history_cursor(), older_before);
        for _ in 0..3 {
            let cursor = app.newer_history_cursor().expect("further newer page");
            let at_boundary = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "reach next newer boundary",
                    input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
                },
                TranscriptFrameStep {
                    label: "request rows at newer edge",
                    input: TranscriptFrameInput::ScrollDown(3),
                },
            ]);
            let previous = at_boundary[1].observation.anchor.as_ref().expect("reader");
            let source = at_boundary[1].observation.terminal_rows
                [at_boundary[1].observation.viewport_top + previous.viewport_row_offset]
                .0
                .clone()
                .expect("reader source");
            let page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "another newer page",
                    input: TranscriptFrameInput::AppendHistory {
                        events: (cursor.sequence..cursor.sequence + 50).map(event).collect(),
                        has_more: true,
                    },
                },
                TranscriptFrameStep {
                    label: "settled newer page",
                    input: TranscriptFrameInput::Observe,
                },
            ]);
            assert_eq!(page[0].observation, page[1].observation);
            assert!(
                page[0]
                    .observation
                    .terminal_rows
                    .iter()
                    .any(|(id, _)| id.as_ref() == Some(&source))
            );
            assert!(app.resident_transcript_event_count() <= 1024);
            assert!(app.older_history_cursor().is_some());
        }
        drop(app);
    }

    #[test]
    fn repeated_bidirectional_pages_keep_reader_and_residency_bounded() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
        };
        let recent = (1_001..=1_040).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "resume",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "reach old edge",
                input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
            },
        ]);
        let mut upper = 1_001_u64;
        for _ in 0..20 {
            let lower = upper - 50;
            let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "load older page",
                    input: TranscriptFrameInput::PrependHistory {
                        events: (lower..upper).map(event).collect(),
                        has_more: lower > 1,
                    },
                },
                TranscriptFrameStep {
                    label: "continue to older edge",
                    input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
                },
            ]);
            assert!(frames[1].observation.semantic_items.len() <= 1_024);
            assert!(app.resident_transcript_event_count() <= 1_024);
            let sync = app.transcript_layout_mut().drain_sync_stats();
            assert!(
                sync.iter()
                    .all(|stats| stats.entries_scanned <= app.transcript().len() + 1)
            );
            upper = lower;
        }
        assert!(app.newer_history_cursor().is_some());
        for _ in 0..15 {
            let cursor = app
                .newer_history_cursor()
                .expect("reloadable newer history");
            let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "move to newer edge",
                    input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
                },
                TranscriptFrameStep {
                    label: "load newer page",
                    input: TranscriptFrameInput::AppendHistory {
                        events: (cursor.sequence..cursor.sequence + 50).map(event).collect(),
                        has_more: true,
                    },
                },
            ]);
            assert!(frames[1].observation.semantic_items.len() <= 1_024);
            assert!(app.resident_transcript_event_count() <= 1_024);
            let sync = app.transcript_layout_mut().drain_sync_stats();
            assert!(
                sync.iter()
                    .all(|stats| stats.entries_scanned <= app.transcript().len() + 1)
            );
        }
        let settled = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "return to live tail",
                input: TranscriptFrameInput::ScrollToBottom,
            },
            TranscriptFrameStep {
                label: "tail settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(settled[0].observation, settled[1].observation);
        assert!(!app.loading_older_history());
        drop(app);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn naturally_evicted_newer_boundary_survives_reversal_and_repeated_frames() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
        };
        let initial = (500..=1_500).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &initial, &[], true);
        TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "reach older boundary",
            input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
        }]);
        let older = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "accept older page and evict newer suffix",
                input: TranscriptFrameInput::PrependHistory {
                    events: (450..500).map(event).collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "older frame settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(older[0].observation, older[1].observation);
        let reader = older[0].observation.anchor.as_ref().expect("older reader");
        let reader_source = older[0].observation.terminal_rows
            [older[0].observation.viewport_top + reader.viewport_row_offset]
            .clone();
        assert!(
            reader_source.0.is_some(),
            "reader has canonical source identity"
        );
        let first_cursor = app.newer_history_cursor().expect("evicted newer suffix");
        let forward = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "reverse toward newer boundary",
                input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "forward frame settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(forward[0].observation, forward[1].observation);
        assert!(
            forward[0]
                .observation
                .terminal_rows
                .contains(&reader_source)
        );
        assert!(app.should_load_newer_history());
        let cursor = app
            .newer_history_cursor()
            .expect("newer boundary after reversal");
        assert!(cursor.sequence >= first_cursor.sequence);
        assert!(app.resident_transcript_event_count() <= 1_024);
        assert_eq!(
            app.resident_transcript_oldest_sequence(),
            Some(450),
            "the forward boundary retains the older resident source"
        );
        let first_page_end = cursor.sequence.saturating_add(511).min(1_500);
        let first_page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "first bounded newer page",
                input: TranscriptFrameInput::AppendHistory {
                    events: (cursor.sequence..=first_page_end).map(event).collect(),
                    has_more: first_page_end < 1_500,
                },
            },
            TranscriptFrameStep {
                label: "first newer page settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(first_page[0].observation, first_page[1].observation);
        assert!(
            first_page[0]
                .observation
                .terminal_rows
                .contains(&reader_source)
        );
        assert!(app.resident_transcript_event_count() <= 1_024);
        if first_page_end < 1_500 {
            let next = app.newer_history_cursor().expect("remaining newer page");
            assert_eq!(next.sequence, first_page_end + 1);
        }
        let last = if first_page_end < 1_500 {
            TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "restore remaining newer history",
                    input: TranscriptFrameInput::AppendHistory {
                        events: (first_page_end + 1..=1_500).map(event).collect(),
                        has_more: false,
                    },
                },
                TranscriptFrameStep {
                    label: "restored frame settled",
                    input: TranscriptFrameInput::Observe,
                },
            ])
        } else {
            first_page
        };
        assert_eq!(last[0].observation, last[1].observation);
        assert!(
            last[0].observation.terminal_rows.len() >= forward[0].observation.terminal_rows.len()
        );
        let tail = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "return to live tail",
                input: TranscriptFrameInput::ScrollToBottom,
            },
            TranscriptFrameStep {
                label: "tail settled",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(tail[0].observation, tail[1].observation);
        assert_eq!(tail[0].observation.scroll_mode, "bottom_follow");
        drop(app);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn older_page_eviction_keeps_reader_frame_and_reloadable_newer_boundary() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: format!("message {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
        };
        let initial = (500..=1500).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &initial, &[], true);
        let before = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "initial",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "reader near oldest",
                input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
            },
        ]);
        assert!(app.should_load_older_history());
        let reader = before[1].observation.anchor.as_ref().expect("reader");
        let reader_source = before[1].observation.terminal_rows
            [before[1].observation.viewport_top + reader.viewport_row_offset]
            .0
            .clone()
            .expect("reader source");
        let reader_row = reader.row_in_item;
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "older page and eviction",
                input: TranscriptFrameInput::PrependHistory {
                    events: (450..500).map(event).collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "stable repeat",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert!(app.resident_transcript_event_count() <= 1024);
        assert!(app.newer_history_cursor().is_some());
        assert_eq!(frames[0].observation, frames[1].observation);
        assert!(frames[0].observation.anchor.is_some());
        assert_eq!(
            frames[0].observation.terminal_items.len(),
            app.transcript().len()
        );
        assert!(frames[0].observation.terminal_items.len() <= 1024);
        assert!(
            frames[0]
                .observation
                .terminal_rows
                .iter()
                .any(|(source, row)| source.as_ref() == Some(&reader_source) && *row == reader_row)
        );
        let cursor = app.newer_history_cursor().expect("evicted newer boundary");
        let older = app.resident_transcript_oldest_sequence();
        let newer = (cursor.sequence..=1500).map(event).collect::<Vec<_>>();
        let reloaded = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "reload newer",
                input: TranscriptFrameInput::AppendHistory {
                    events: newer,
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "repeat reload",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert!(app.newer_history_cursor().is_none());
        assert_eq!(app.resident_transcript_oldest_sequence(), older);
        assert_eq!(reloaded[0].observation, reloaded[1].observation);
        // Repeat the same traversal after restoring the suffix. Each older
        // page evicts a complete off-screen suffix, without losing the reader.
        for start in [400, 350, 300] {
            let at_boundary =
                TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
                    label: "scroll to next older boundary",
                    input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
                }]);
            let anchor = at_boundary[0]
                .observation
                .anchor
                .as_ref()
                .expect("reader before page");
            let source = at_boundary[0].observation.terminal_rows
                [at_boundary[0].observation.viewport_top + anchor.viewport_row_offset]
                .0
                .clone()
                .expect("reader source before page");
            let row = anchor.row_in_item;
            let page = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "next older page",
                    input: TranscriptFrameInput::PrependHistory {
                        events: (start..start + 50).map(event).collect(),
                        has_more: true,
                    },
                },
                TranscriptFrameStep {
                    label: "settled older page",
                    input: TranscriptFrameInput::Observe,
                },
            ]);
            assert_eq!(page[0].observation, page[1].observation);
            assert!(
                page[0]
                    .observation
                    .terminal_rows
                    .iter()
                    .any(|(id, index)| id.as_ref() == Some(&source) && *index == row)
            );
            assert!(app.resident_transcript_event_count() <= 1024);
            assert!(page[0].observation.terminal_items.len() <= 1024);
            let cursor = app.newer_history_cursor().expect("reloadable suffix");
            let reload = TranscriptFrameSequence::new(&mut app, 80, 12).run([
                TranscriptFrameStep {
                    label: "restore suffix",
                    input: TranscriptFrameInput::AppendHistory {
                        events: (cursor.sequence..=1500).map(event).collect(),
                        has_more: false,
                    },
                },
                TranscriptFrameStep {
                    label: "settled restored suffix",
                    input: TranscriptFrameInput::Observe,
                },
            ]);
            assert_eq!(reload[0].observation, reload[1].observation);
        }
        assert!(app.newer_history_cursor().is_none());
        let down = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "navigate forward through restored resident rows",
                input: TranscriptFrameInput::ScrollDown(18),
            },
            TranscriptFrameStep {
                label: "settled forward navigation",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(down[0].observation, down[1].observation);
        assert!(down[0].observation.viewport_top > 0);
        let tail = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "return to live tail",
                input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "stable live tail",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(tail[0].observation, tail[1].observation);
        // Explicitly pin the live tail after paging; virtual bottom space can
        // leave an ordinary large scroll detached even with no newer cursor.
        app.scroll_transcript_to_bottom();
        let pinned = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "pinned live tail",
            input: TranscriptFrameInput::Observe,
        }]);
        assert_eq!(pinned[0].observation.scroll_mode, "bottom_follow");
        drop(app);
    }

    #[test]
    fn resumed_history_page_reveals_only_requested_rows_once() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let recent = (30..42).map(event).collect::<Vec<_>>();
        let older = (1..30).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        let initial = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "resumed tail",
            input: TranscriptFrameInput::Observe,
        }]);
        let to_near_top = initial[0].observation.viewport_top.saturating_sub(1);
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "near boundary",
                input: TranscriptFrameInput::ScrollUp(to_near_top),
            },
            TranscriptFrameStep {
                label: "loading at boundary",
                input: TranscriptFrameInput::ScrollUp(2),
            },
            TranscriptFrameStep {
                label: "older history accepted",
                input: TranscriptFrameInput::PrependHistory {
                    events: older,
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "next accepted frame",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        let before = &frames[0].observation;
        let after = &frames[2].observation;
        // Boundary input asks to reveal the newly available older item; the
        // accepted frame must not stay on the former first resident message.
        assert_ne!(
            before.anchor.as_ref().map(|anchor| anchor.item_id),
            after.anchor.as_ref().map(|anchor| anchor.item_id)
        );
        let source = before
            .terminal_rows
            .get(
                before
                    .viewport_top
                    .saturating_add(before.anchor.as_ref().expect("anchor").viewport_row_offset),
            )
            .and_then(|(source, _)| source.as_ref())
            .expect("source identity before boundary");
        let previous_row = after
            .terminal_rows
            .iter()
            .position(|(id, row)| {
                id.as_ref() == Some(source)
                    && *row == before.anchor.as_ref().expect("anchor").row_in_item
            })
            .expect("previous item survives prepend");
        assert!(previous_row >= after.viewport_top);
        assert!(previous_row - after.viewport_top <= 2);
        assert_eq!(after, &frames[3].observation);
    }

    #[test]
    fn resumed_boundary_reversal_preserves_reader_across_large_page_and_resize() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}: {}", "wide content ".repeat(8)),
                },
            )
        };
        let recent = (30..42).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        let initial = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "resumed tail",
            input: TranscriptFrameInput::Observe,
        }]);
        let top = initial[0].observation.viewport_top;
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "reach boundary",
                input: TranscriptFrameInput::ScrollUp(top),
            },
            TranscriptFrameStep {
                label: "ask for older rows",
                input: TranscriptFrameInput::ScrollUp(5),
            },
            TranscriptFrameStep {
                label: "more input while loading",
                input: TranscriptFrameInput::ScrollUp(3),
            },
            TranscriptFrameStep {
                label: "reverse before page arrives",
                input: TranscriptFrameInput::ScrollDown(2),
            },
            TranscriptFrameStep {
                label: "large older page arrives",
                input: TranscriptFrameInput::PrependHistory {
                    events: (1..30).map(event).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "rerender at same width",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "reflow at narrower width",
                input: TranscriptFrameInput::Resize(52, 12),
            },
        ]);
        let before = &frames[3].observation;
        let after = &frames[4].observation;
        let anchor = before.anchor.as_ref().expect("reader has source anchor");
        let source = before
            .terminal_rows
            .get(before.viewport_top + anchor.viewport_row_offset)
            .and_then(|(source, _)| source.as_ref())
            .expect("reading source before page");
        let source_row_after = after
            .terminal_rows
            .iter()
            .position(|(id, row)| id.as_ref() == Some(source) && *row == anchor.row_in_item)
            .expect("same source row survives large page");
        assert_eq!(
            source_row_after - after.viewport_top,
            anchor.viewport_row_offset
        );
        assert_eq!(after, &frames[5].observation);
        assert_eq!(
            frames[6].observation.anchor.as_ref().map(|a| a.item_id),
            Some(anchor.item_id)
        );
    }

    #[test]
    fn resumed_boundary_multiple_pages_preserve_newer_navigation_and_tail() {
        let session_id = SessionId::new();
        let event = |sequence| {
            durable(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            )
        };
        let recent = (60..72).map(event).collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &recent, &[], true);
        let initial = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "resumed tail",
            input: TranscriptFrameInput::Observe,
        }]);
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "first older boundary",
                input: TranscriptFrameInput::ScrollUp(initial[0].observation.viewport_top + 3),
            },
            TranscriptFrameStep {
                label: "first page",
                input: TranscriptFrameInput::PrependHistory {
                    events: (40..60).map(event).collect(),
                    has_more: true,
                },
            },
            TranscriptFrameStep {
                label: "first page again",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "second older boundary",
                input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "second page",
                input: TranscriptFrameInput::PrependHistory {
                    events: (20..40).map(event).collect(),
                    has_more: false,
                },
            },
            TranscriptFrameStep {
                label: "reverse to live tail",
                input: TranscriptFrameInput::ScrollDown(usize::MAX / 4),
            },
            TranscriptFrameStep {
                label: "tail accepted again",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(frames[1].observation, frames[2].observation);
        let after_second_page = &frames[4].observation;
        assert!(after_second_page.anchor.is_some());
        assert_eq!(frames[5].observation, frames[6].observation);
        assert!(frames[5].observation.viewport_top > after_second_page.viewport_top);
        assert!(frames[5].observation.terminal_items.len() >= recent.len());
    }

    #[test]
    fn ephemeral_notice_remains_between_durable_frames_after_resize() {
        let session_id = SessionId::new();
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(
                Some(session_id),
                &[durable(
                    session_id,
                    1,
                    SessionEventKind::AssistantMessage {
                        text: "before".to_owned(),
                    },
                )],
                &[],
                false,
            ),
            40,
            10,
        )
        .run([
            TranscriptFrameStep {
                label: "local issue",
                input: TranscriptFrameInput::EphemeralPlain("local issue".to_owned()),
            },
            TranscriptFrameStep {
                label: "later durable",
                input: durable_input(durable(
                    session_id,
                    2,
                    SessionEventKind::AssistantMessage {
                        text: "after".to_owned(),
                    },
                )),
            },
            TranscriptFrameStep {
                label: "resized",
                input: TranscriptFrameInput::Resize(24, 8),
            },
        ]);

        let final_frame = frames.last().expect("last frame");
        assert_eq!(
            final_frame
                .observation
                .terminal_items
                .iter()
                .filter(|(_, source, _)| source.is_some())
                .count(),
            2
        );
        assert!(final_frame.observation.terminal_items.len() >= 3);
    }

    fn filesystem_plugin_host() -> bcode_plugin::PluginHost {
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static filesystem plugin manifest should parse");
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("static filesystem plugin should load")
    }

    fn shell_plugin_host() -> bcode_plugin::PluginHost {
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("static shell plugin manifest should parse");
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("static shell plugin should load")
    }

    fn durable(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        }
    }

    #[test]
    fn cancelled_default_stream_stops_presentation_and_keeps_exact_text() {
        let session_id = SessionId::new();
        let accepted = "cancelled multi-paragraph output\n\nwith an exact final prefix";
        let frames = {
            let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
            let sequence = TranscriptFrameSequence::new(&mut app, 80, 24);
            assert!(
                !sequence
                    .app
                    .apply_presentation_config(bcode_config::PresentationConfig::default())
            );
            sequence.run([
                TranscriptFrameStep {
                    label: "stream-start",
                    input: live_input(SessionLiveEvent {
                        session_id,
                        kind:
                            bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                                output_position: None,
                                turn_id: "turn-cancelled".to_owned(),
                                segment_id: "segment-1".to_owned(),
                                segment_order: 0,
                                update: TextStreamUpdate {
                                    generation: 0,
                                    first_revision: 1,
                                    revision: 1,
                                    operation: TextStreamOperation::Append {
                                        expected_offset: 0,
                                        text: accepted.to_owned(),
                                    },
                                },
                            },
                    }),
                },
                TranscriptFrameStep {
                    label: "turn-cancelled",
                    input: durable_input(durable(
                        session_id,
                        1,
                        SessionEventKind::ModelTurnFinished {
                            turn_id: "turn-cancelled".to_owned(),
                            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
                            message: None,
                        },
                    )),
                },
                TranscriptFrameStep {
                    label: "presentation-stopped",
                    input: TranscriptFrameInput::AssertNoPendingStreaming,
                },
            ])
        };

        assert_eq!(frames.len(), 3);
        assert!(!frames[0].text.contains("cancelled multi-paragraph output"));
        assert!(
            frames[1].text.contains("cancelled multi-paragraph output"),
            "{}",
            frames[1].text
        );
        assert!(
            frames[1].text.contains("with an exact final prefix"),
            "{}",
            frames[1].text
        );
        assert_eq!(
            frames[1].observation.semantic_items,
            frames[2].observation.semantic_items
        );
        assert!(frames[2].text.contains("cancelled multi-paragraph output"));
        assert!(frames[2].text.contains("with an exact final prefix"));
    }

    #[test]
    fn smoothed_stream_frame_sequence_preserves_identity_markdown_and_following() {
        let session_id = SessionId::new();
        let frames = {
            let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
            let sequence = TranscriptFrameSequence::new(&mut app, 80, 24);
            assert!(
                !sequence
                    .app
                    .apply_presentation_config(bcode_config::PresentationConfig::default())
            );
            sequence.run([
                TranscriptFrameStep {
                    label: "stream-start",
                    input: live_input(SessionLiveEvent {
                        session_id,
                        kind:
                            bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                                output_position: None,
                                turn_id: "turn-1".to_owned(),
                                segment_id: "segment-1".to_owned(),
                                segment_order: 0,
                                update: TextStreamUpdate {
                                    generation: 0,
                                    first_revision: 1,
                                    revision: 1,
                                    operation: TextStreamOperation::Append {
                                        expected_offset: 0,
                                        text: "**smooth markdown output**".to_owned(),
                                    },
                                },
                            },
                    }),
                },
                TranscriptFrameStep {
                    label: "stream-complete",
                    input: TranscriptFrameInput::AdvanceStreaming(
                        std::time::Duration::from_millis(100),
                    ),
                },
            ])
        };
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].observation.semantic_items.len(), 1);
        assert_eq!(frames[1].observation.semantic_items.len(), 1);
        assert_eq!(
            frames[0].observation.terminal_items[0].0,
            frames[1].observation.terminal_items[0].0
        );
        assert!(
            frames[1].text.contains("smooth markdown output"),
            "{}",
            frames[1].text
        );
        assert!(frames[1].markdown_fresh_equivalent);
        assert_ne!(frames[1].observation.scroll_mode, "manual_detached");
        assert!(matches!(
            frames[1].observation.damage,
            super::super::transcript_document::TranscriptDocumentDamage::Items(ref ids)
                if ids.len() == 1
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One scenario proves all per-frame observation dimensions together.
    fn frame_sequence_captures_semantic_terminal_damage_and_viewport_state() {
        let session_id = SessionId::new();
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "initial",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "user",
                input: durable_input(durable(
                    session_id,
                    1,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: "hello".to_owned(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )),
            },
            TranscriptFrameStep {
                label: "assistant-first",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        segment_id: "segment-0".to_owned(),
                        segment_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 1,
                            revision: 1,
                            operation: TextStreamOperation::Append {
                                expected_offset: 0,
                                text: "first".to_owned(),
                            },
                        },
                    },
                }),
            },
            TranscriptFrameStep {
                label: "assistant-second",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        segment_id: "segment-0".to_owned(),
                        segment_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 2,
                            revision: 2,
                            operation: TextStreamOperation::Append {
                                expected_offset: 5,
                                text: " second".to_owned(),
                            },
                        },
                    },
                }),
            },
        ]);

        assert_eq!(frames.len(), 4);
        assert!(frames[1].text.contains("hello"), "{}", frames[1].text);
        assert_eq!(frames[1].observation.semantic_items.len(), 1);
        assert_eq!(frames[1].observation.terminal_items.len(), 1);
        assert!(!frames[1].observation.terminal_rows.is_empty());
        assert!(matches!(
            frames[1].observation.damage,
            super::super::transcript_document::TranscriptDocumentDamage::Structural
        ));
        assert_eq!(frames[2].observation.semantic_items.len(), 2);
        assert_eq!(frames[2].observation.terminal_items.len(), 2);
        assert!(matches!(
            frames[3].observation.damage,
            super::super::transcript_document::TranscriptDocumentDamage::Items(ref ids)
                if ids.len() == 1
        ));
        assert_eq!(
            frames[2].observation.terminal_items[1].0, frames[3].observation.terminal_items[1].0,
            "terminal item identity changed across one semantic revision"
        );
        assert_eq!(
            frames[3].observation.terminal_items[1].2,
            Some(frames[3].observation.semantic_items[1].1)
        );
        assert!(
            frames[3].text.contains("first second"),
            "{}",
            frames[3].text
        );
        assert!(
            frames
                .iter()
                .all(|frame| frame.observation.scroll_mode != "manual_detached")
        );
        assert_no_forbidden_frames(&frames, |frame| {
            frame
                .text
                .contains("forbidden raw json")
                .then(|| "raw JSON flashed".to_owned())
        })
        .expect("frame sequence should remain rich");
    }

    #[test]
    fn assistant_frames_preserve_every_accepted_cumulative_prefix() {
        let session_id = SessionId::new();
        let chunks = ["Leading ", "words and ", "trailing chars ✓"];
        let mut expected_offset = 0_usize;
        let steps = chunks.into_iter().enumerate().map(|(index, text)| {
            let revision = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            let step = TranscriptFrameStep {
                label: match index {
                    0 => "assistant-leading",
                    1 => "assistant-middle",
                    _ => "assistant-trailing",
                },
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        segment_id: "segment-0".to_owned(),
                        segment_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: revision,
                            revision,
                            operation: TextStreamOperation::Append {
                                expected_offset,
                                text: text.to_owned(),
                            },
                        },
                    },
                }),
            };
            expected_offset = expected_offset.saturating_add(text.len());
            step
        });
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run(steps);
        let mut expected = String::new();
        for (frame, chunk) in frames.iter().zip(chunks) {
            expected.push_str(chunk);
            assert!(
                frame.text.contains(&expected),
                "{}\n{}",
                frame.label,
                frame.text
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn markdown_stream_scroll_resize_and_terminal_projection_converge_exactly() {
        let session_id = SessionId::new();
        let first = format!("# Streaming report\n\n{}", "context row\n\n".repeat(24));
        let second = "- [x] retained projection\n- [ ] final convergence\n\n";
        let final_suffix = "[guide](https://example.com)\n";
        let first_len = first.len();
        let second_offset = first_len.saturating_add(second.len());
        let update = |revision, expected_offset, text: String| {
            live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                    output_position: None,
                    turn_id: "turn-markdown".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    update: TextStreamUpdate {
                        generation: 0,
                        first_revision: revision,
                        revision,
                        operation: TextStreamOperation::Append {
                            expected_offset,
                            text,
                        },
                    },
                },
            })
        };
        let terminal = live_input(SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-markdown".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: TextStreamUpdate {
                    generation: 0,
                    first_revision: 4,
                    revision: 4,
                    operation: TextStreamOperation::Terminal {
                        status: bcode_session_models::TextStreamTerminalStatus::Completed,
                    },
                },
            },
        });
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            72,
            14,
        )
        .run([
            TranscriptFrameStep {
                label: "first-formatted-projection",
                input: update(1, 0, first),
            },
            TranscriptFrameStep {
                label: "detached-during-stream",
                input: TranscriptFrameInput::ScrollUp(5),
            },
            TranscriptFrameStep {
                label: "second-formatted-projection",
                input: update(2, first_len, second.to_owned()),
            },
            TranscriptFrameStep {
                label: "narrow-projection",
                input: TranscriptFrameInput::Resize(38, 14),
            },
            TranscriptFrameStep {
                label: "final-streaming-projection",
                input: update(3, second_offset, final_suffix.to_owned()),
            },
            TranscriptFrameStep {
                label: "terminal-projection",
                input: terminal,
            },
        ]);

        assert!(frames[0].text.contains("context row"));
        for frame in &frames[1..] {
            assert_eq!(frame.observation.scroll_mode, "manual_detached");
        }
        assert!(
            frames[2].observation.terminal_rows.len() >= frames[1].observation.terminal_rows.len()
        );
        assert_eq!(frames[2].observation.anchor, frames[1].observation.anchor);
        assert_eq!(frames[3].observation.anchor, frames[1].observation.anchor);
        let terminal_item = frames
            .last()
            .and_then(|frame| frame.observation.terminal_items.last())
            .expect("terminal Markdown item");
        let semantic_item = frames
            .last()
            .and_then(|frame| frame.observation.semantic_items.last())
            .expect("semantic Markdown item");
        assert_eq!(terminal_item.2, Some(semantic_item.1));
        assert!(
            frames
                .last()
                .is_some_and(|frame| frame.markdown_fresh_equivalent)
        );
        assert!(frames.last().is_some_and(|frame| {
            frame.text.contains("guide") || frame.observation.anchor.is_some()
        }));
    }

    #[test]
    fn structured_reasoning_is_visible_on_each_live_part_revision() {
        let session_id = SessionId::new();
        let update = |revision, expected_offset, text: &str| {
            TranscriptFrameStep {
            label: if revision == 1 {
                "reasoning-first-part"
            } else {
                "reasoning-second-part"
            },
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    activity_id: "reasoning-1".to_owned(),
                    activity_order: 0,
                    part_id: "summary-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    part_order: 0,
                    update: TextStreamUpdate {
                        generation: 0,
                        first_revision: revision,
                        revision,
                        operation: TextStreamOperation::Append {
                            expected_offset,
                            text: text.to_owned(),
                        },
                    },
                },
            }),
        }
        };
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            update(1, 0, "Immediate reasoning"),
            update(2, 19, " continues"),
        ]);
        assert!(
            frames[0].text.contains("Immediate reasoning"),
            "{}",
            frames[0].text
        );
        assert!(
            frames[1].text.contains("Immediate reasoning continues"),
            "{}",
            frames[1].text
        );
        assert_eq!(frames[1].observation.semantic_items.len(), 1);
        assert_eq!(frames[1].observation.terminal_items.len(), 1);
        assert_eq!(
            frames[0].observation.terminal_items[0].0,
            frames[1].observation.terminal_items[0].0
        );
    }

    /// Bedrock-shaped raw reasoning must reach drawn terminal rows and survive turn completion.
    ///
    /// This is the rendered-frame counterpart to the provider-boundary verification: the plugin
    /// emits `PartDelta` for each `thinking_delta`, then an authoritative `PartCompleted` at
    /// `content_block_stop`. Both must leave readable text on screen.
    #[test]
    fn bedrock_raw_reasoning_streams_and_persists_in_drawn_frames() {
        let session_id = SessionId::new();
        let activity_id = "bedrock-messages-reasoning-0";
        let part_id = "raw-0";
        let delta = |label: &'static str, revision: u64, expected_offset: usize, text: &str| {
            TranscriptFrameStep {
                label,
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        activity_id: activity_id.to_owned(),
                        activity_order: 0,
                        part_id: part_id.to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        part_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: revision,
                            revision,
                            operation: TextStreamOperation::Append {
                                expected_offset,
                                text: text.to_owned(),
                            },
                        },
                    },
                }),
            }
        };
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            delta("thinking-delta-1", 1, 0, "Solving for the ball price"),
            delta("thinking-delta-2", 2, 26, " algebraically"),
            // `content_block_stop` finishes the activity; readable text must remain.
            TranscriptFrameStep {
                label: "reasoning-finished",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningActivity {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        event: bcode_session_models::ReasoningActivityEvent::Finished {
                            activity_id: activity_id.to_owned(),
                            activity_order: 0,
                            status: bcode_session_models::ReasoningActivityStatus::Completed,
                        },
                    },
                }),
            },
        ]);

        assert!(
            frames[0].text.contains("Solving for the ball price"),
            "first reasoning delta must be drawn: {}",
            frames[0].text
        );
        assert!(
            frames[1]
                .text
                .contains("Solving for the ball price algebraically"),
            "appended reasoning must be drawn: {}",
            frames[1].text
        );
        assert!(
            frames[2]
                .text
                .contains("Solving for the ball price algebraically"),
            "finishing the activity must not erase readable reasoning: {}",
            frames[2].text
        );
        // No frame may show the bare heading with an empty body, which was the original defect.
        assert!(
            !frames.iter().any(|frame| frame.text.contains("Reasoning")
                && !frame.text.contains("Solving for the ball price")),
            "no frame may render reasoning chrome without its readable body"
        );
    }

    /// Replayed durable reasoning must render from canonical history alone.
    ///
    /// This covers the reattach path: a fresh app with no live stream state receives the persisted
    /// `AssistantReasoningActivity` and must draw its readable parts.
    #[test]
    fn replayed_durable_reasoning_renders_without_live_stream_state() {
        let session_id = SessionId::new();
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([TranscriptFrameStep {
            label: "durable-reasoning-replay",
            input: durable_input(SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: "bedrock-messages-reasoning-0".to_owned(),
                        order: 0,
                        status: bcode_session_models::ReasoningActivityStatus::Completed,
                        parts: vec![bcode_session_models::ReasoningPart {
                            part_id: "raw-0".to_owned(),
                            kind: bcode_session_models::ReasoningContentKind::Raw,
                            role: bcode_session_models::ReasoningContentRole::Detail,
                            order: 0,
                            text: "Replayed reasoning detail".to_owned(),
                        }],
                        opaque: false,
                    },
                },
            }),
        }]);

        assert!(
            frames[0].text.contains("Replayed reasoning detail"),
            "durable reasoning must replay into drawn rows: {}",
            frames[0].text
        );
    }

    /// An opaque-only activity must render an explanation, never a bare heading.
    ///
    /// This is the rendered-frame proof for the withheld-reasoning case: the provider recorded
    /// opaque evidence and no readable text, so the transcript must say so.
    #[test]
    fn opaque_only_reasoning_renders_explanatory_chrome_in_drawn_frames() {
        let session_id = SessionId::new();
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([TranscriptFrameStep {
            label: "opaque-only-reasoning",
            input: durable_input(SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: SessionEventKind::AssistantReasoningActivity {
                    turn_id: "turn-1".to_owned(),
                    activity: bcode_session_models::ReasoningActivity {
                        activity_id: "bedrock-messages-reasoning-0".to_owned(),
                        order: 0,
                        status: bcode_session_models::ReasoningActivityStatus::Completed,
                        parts: Vec::new(),
                        opaque: true,
                    },
                },
            }),
        }]);

        assert!(
            frames[0].text.contains("did not return readable reasoning"),
            "opaque-only reasoning must be explained on screen: {}",
            frames[0].text
        );
    }

    fn raw_reasoning_event(session_id: SessionId) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "bedrock-messages-reasoning-0".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: vec![bcode_session_models::ReasoningPart {
                        part_id: "raw-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Raw,
                        role: bcode_session_models::ReasoningContentRole::Detail,
                        order: 0,
                        text: "Raw chain of thought".to_owned(),
                    }],
                    opaque: false,
                },
            },
        }
    }

    /// Display modes filter readable content without leaving a blank heading.
    #[test]
    fn thinking_display_modes_change_drawn_reasoning_without_blank_frames() {
        let session_id = SessionId::new();
        let reasoning = raw_reasoning_event(session_id);
        // `all` and `raw` both select raw parts, so the text is drawn.
        for mode in [
            bcode_config::TuiThinkingMode::All,
            bcode_config::TuiThinkingMode::Raw,
        ] {
            let frames = TranscriptFrameSequence::new(
                &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
                100,
                40,
            )
            .run([
                TranscriptFrameStep {
                    label: "reasoning-display-mode",
                    input: TranscriptFrameInput::ReasoningDisplayMode(mode),
                },
                TranscriptFrameStep {
                    label: "reasoning-visible",
                    input: durable_input(reasoning.clone()),
                },
            ]);
            assert!(
                frames[1].text.contains("Raw chain of thought"),
                "{mode:?} must draw raw reasoning: {}",
                frames[1].text
            );
        }

        // `summary` excludes raw parts: the activity must explain the local filter, not go blank.
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "reasoning-before-summary-filter",
                input: durable_input(reasoning),
            },
            TranscriptFrameStep {
                label: "reasoning-filtered-by-summary-mode",
                input: TranscriptFrameInput::ReasoningDisplayMode(
                    bcode_config::TuiThinkingMode::Summary,
                ),
            },
        ]);
        assert!(
            !frames[1].text.contains("Raw chain of thought"),
            "summary mode must not draw raw reasoning: {}",
            frames[1].text
        );
        assert!(
            frames[1].text.contains("display setting"),
            "filtered reasoning must explain the local display choice: {}",
            frames[1].text
        );
    }

    #[test]
    fn thinking_config_changes_refresh_existing_reasoning_frames() {
        let session_id = SessionId::new();
        let reasoning = raw_reasoning_event(session_id);
        // Applying configuration must refresh the mode even when visibility is unchanged.
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "reasoning-before-config",
                input: durable_input(reasoning.clone()),
            },
            TranscriptFrameStep {
                label: "reasoning-after-config",
                input: TranscriptFrameInput::ThinkingConfig(bcode_config::TuiThinkingConfig {
                    mode: bcode_config::TuiThinkingMode::Summary,
                    show: true,
                }),
            },
        ]);
        assert!(frames[0].text.contains("Raw chain of thought"));
        assert!(!frames[1].text.contains("Raw chain of thought"));
        assert!(frames[1].text.contains("display setting"));

        // `/thinking hide` removes the reasoning item entirely.
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "reasoning-before-hide",
                input: durable_input(reasoning),
            },
            TranscriptFrameStep {
                label: "reasoning-hidden",
                input: TranscriptFrameInput::ThinkingConfig(bcode_config::TuiThinkingConfig {
                    mode: bcode_config::TuiThinkingMode::Raw,
                    show: false,
                }),
            },
            TranscriptFrameStep {
                label: "reasoning-restored",
                input: TranscriptFrameInput::ThinkingConfig(bcode_config::TuiThinkingConfig {
                    mode: bcode_config::TuiThinkingMode::Raw,
                    show: true,
                }),
            },
        ]);
        assert!(frames[0].text.contains("Raw chain of thought"));
        assert!(
            !frames[1].text.contains("Raw chain of thought"),
            "hidden reasoning must not draw its text: {}",
            frames[1].text
        );
        assert_eq!(frames[1].observation.semantic_items.len(), 1);
        assert!(frames[1].observation.terminal_items.is_empty());
        assert!(frames[2].text.contains("Raw chain of thought"));
        assert_eq!(
            frames[0].observation.semantic_items[0].0,
            frames[2].observation.semantic_items[0].0
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One transition fixture proves every draft-to-result frame.
    fn filesystem_handoff_has_no_blank_or_raw_json_frame() {
        let session_id = SessionId::new();
        let draft = |revision, contents: &str| SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                event: bcode_session_models::ToolRequestDraftEvent {
                    output_position: None,
                    turn_id: "turn-1".to_owned(),
                    tool_call_id: "call-write".to_owned(),
                    tool_name: "filesystem.write".to_owned(),
                    producer_plugin_id: Some("bcode.filesystem".to_owned()),
                    schema: "bcode.filesystem.request-draft.write".to_owned(),
                    schema_version: 1,
                    placement: bcode_session_models::ToolContributionPlacement::Result,
                    generation: 1,
                    revision,
                    operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                        start_offset: 0,
                        text: serde_json::json!({
                            "path": "src/lib.rs",
                            "contents": contents
                        })
                        .to_string(),
                    },
                    argument_bytes: contents.len().saturating_add(35),
                    truncated: false,
                },
            },
        };
        let request = durable(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-write".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.write".to_owned(),
                arguments_json: r#"{"path":"src/lib.rs","contents":"hello"}"#.to_owned(),
                working_directory: None,
            },
        );
        let result = durable(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-write".to_owned(),
                    model_output: "wrote 5 bytes".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "call-write-filesystem-change".to_owned(),
                            producer_plugin_id: "bcode.filesystem".to_owned(),
                            schema: "bcode.filesystem.change".to_owned(),
                            schema_version: 1,
                            tool_call_id: Some("call-write".to_owned()),
                            title: Some("File change".to_owned()),
                            metadata: serde_json::json!({
                                "tool_name": "filesystem.write",
                                "summary": "wrote 5 bytes",
                                "path": "src/lib.rs",
                                "old_text": "",
                                "new_text": "hello",
                                "old_start_line": 1,
                                "new_start_line": 1
                            }),
                            refs: Vec::new(),
                        }),
                    }),
                    content: Vec::new(),
                },
            },
        );
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .with_plugin_host(Arc::new(filesystem_plugin_host()))
        .run([
            TranscriptFrameStep {
                label: "draft-first",
                input: live_input(draft(1, "hello")),
            },
            TranscriptFrameStep {
                label: "draft-second",
                input: live_input(draft(2, "hello world")),
            },
            TranscriptFrameStep {
                label: "accepted-request",
                input: durable_input(request),
            },
            TranscriptFrameStep {
                label: "result",
                input: durable_input(result),
            },
        ]);
        assert_no_forbidden_frames(&frames, |frame| {
            let raw_json = frame.text.contains(r#""contents":"hello""#);
            let blank = !frame.text.contains("src/lib.rs") && !frame.text.contains("hello");
            (raw_json || blank).then(|| "blank or raw JSON tool frame".to_owned())
        })
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(frames[0].text.contains("assembling"), "{}", frames[0].text);
        assert!(frames[1].text.contains("hello world"), "{}", frames[1].text);
        assert!(frames[3].text.contains("File change"), "{}", frames[3].text);
        assert_eq!(
            frames[0].observation.terminal_items[0].0,
            frames[3].observation.terminal_items[0].0
        );
    }

    #[test]
    fn fast_operation_first_draw_is_only_the_final_invocation_frame() {
        let session_id = SessionId::new();
        let events = vec![
            durable(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-fast".to_owned(),
                    producer_plugin_id: Some("example.plugin".to_owned()),
                    tool_name: "example.fast".to_owned(),
                    arguments_json: r#"{"transient":"must-not-flash"}"#.to_owned(),
                    working_directory: None,
                },
            ),
            durable(
                session_id,
                2,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: "call-fast".to_owned(),
                        model_output: "completed immediately".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                        content: Vec::new(),
                    },
                },
            ),
        ];

        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([TranscriptFrameStep {
            label: "final-only",
            input: TranscriptFrameInput::DurableBatch(events),
        }]);
        let frame = &frames[0];
        assert!(
            frame.text.contains("completed immediately"),
            "{}",
            frame.text
        );
        assert!(!frame.text.contains("must-not-flash"), "{}", frame.text);
        assert!(!frame.text.contains("requested"), "{}", frame.text);
        assert_eq!(frame.observation.semantic_items.len(), 1);
        assert_eq!(frame.observation.terminal_items.len(), 1);
        assert_eq!(
            frame.observation.semantic_items[0].0,
            bcode_session_view_models::TranscriptViewItemId::tool("call-fast")
        );
        assert_eq!(
            frame.observation.terminal_items[0].1.as_ref(),
            Some(&frame.observation.semantic_items[0].0)
        );
    }

    #[test]
    fn coalesced_shell_draft_checkpoint_is_independently_renderable() {
        let session_id = SessionId::new();
        let preview = r#"{"command":"cargo check --workspace","cwd":"/tmp/project"}"#;
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .with_plugin_host(Arc::new(shell_plugin_host()))
        .run([TranscriptFrameStep {
            label: "coalesced-shell-draft",
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-shell".to_owned(),
                        tool_call_id: "call-shell".to_owned(),
                        tool_name: "shell.run".to_owned(),
                        producer_plugin_id: Some("bcode.shell".to_owned()),
                        schema: "bcode.tool.request.shell.run".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation: 1,
                        revision: 7,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: preview.to_owned(),
                        },
                        argument_bytes: preview.len(),
                        truncated: false,
                    },
                },
            }),
        }]);

        assert!(
            frames[0].text.contains("cargo check --workspace"),
            "{}",
            frames[0].text
        );
        assert!(
            frames[0].text.contains("/tmp/project"),
            "{}",
            frames[0].text
        );
        assert!(frames[0].text.contains("assembling"), "{}", frames[0].text);
        assert!(
            !frames[0].text.contains("{\"command\""),
            "{}",
            frames[0].text
        );
        assert_eq!(frames[0].observation.semantic_items.len(), 1);
        assert_eq!(frames[0].observation.terminal_items.len(), 1);
    }

    #[test]
    fn atomic_shell_request_has_no_false_progressive_frame() {
        let session_id = SessionId::new();
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .with_plugin_host(Arc::new(shell_plugin_host()))
        .run([TranscriptFrameStep {
            label: "atomic-shell-request",
            input: durable_input(durable(
                session_id,
                1,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "call-shell".to_owned(),
                    producer_plugin_id: Some("bcode.shell".to_owned()),
                    tool_name: "shell.run".to_owned(),
                    arguments_json: serde_json::json!({
                        "command": "pwd",
                        "cwd": "/tmp/project"
                    })
                    .to_string(),
                    working_directory: Some(std::path::PathBuf::from("/tmp/project")),
                },
            )),
        }]);

        assert!(frames[0].text.contains("❯ pwd"), "{}", frames[0].text);
        assert!(
            frames[0].text.contains("/tmp/project"),
            "{}",
            frames[0].text
        );
        assert!(!frames[0].text.contains("assembling"), "{}", frames[0].text);
        assert!(
            !frames[0].text.contains("Tool · shell.run"),
            "{}",
            frames[0].text
        );
    }

    fn shell_streamed_draft_step(
        session_id: SessionId,
        generation: u64,
        revision: u64,
        preview: &str,
    ) -> TranscriptFrameStep {
        TranscriptFrameStep {
            label: "streamed-draft",
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: Some(bcode_session_models::TurnOutputPosition::new(
                            revision,
                        )),
                        turn_id: "turn-long-shell".to_owned(),
                        tool_call_id: "call-long-shell".to_owned(),
                        tool_name: "shell.run".to_owned(),
                        producer_plugin_id: Some("bcode.shell".to_owned()),
                        schema: "bcode.tool.request.shell.run".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation,
                        revision,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: preview.to_owned(),
                        },
                        argument_bytes: preview.len(),
                        truncated: false,
                    },
                },
            }),
        }
    }

    #[test]
    fn long_shell_draft_frames_survive_resize_idle_and_delayed_updates() {
        let session_id = SessionId::new();
        let command = "printf '%s\\n' alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega";
        let arguments =
            serde_json::json!({"command": command, "cwd": "/tmp/project", "format_commands": true})
                .to_string();
        let draft = |generation, revision, preview: &str| {
            shell_streamed_draft_step(session_id, generation, revision, preview)
        };
        let observe = || TranscriptFrameStep {
            label: "idle",
            input: TranscriptFrameInput::Observe,
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(&mut app, 72, 30)
            .with_plugin_host(std::sync::Arc::new(shell_plugin_host()))
            .run([
                draft(1, 1, r#"{"command":"printf '%s\\n' alpha beta gamma"#),
                draft(1, 2, &arguments),
                observe(),
                TranscriptFrameStep {
                    label: "narrow",
                    input: TranscriptFrameInput::Resize(36, 30),
                },
                observe(),
                draft(2, 1, &arguments),
                observe(),
                TranscriptFrameStep {
                    label: "assembled",
                    input: durable_input(durable(
                        session_id,
                        1,
                        SessionEventKind::ToolCallRequested {
                            tool_call_id: "call-long-shell".to_owned(),
                            producer_plugin_id: Some("bcode.shell".to_owned()),
                            tool_name: "shell.run".to_owned(),
                            arguments_json: arguments,
                            working_directory: None,
                        },
                    )),
                },
                draft(3, 99, r#"{"command":"obsolete"}"#),
                observe(),
                TranscriptFrameStep {
                    label: "wide",
                    input: TranscriptFrameInput::Resize(72, 30),
                },
                observe(),
                TranscriptFrameStep {
                    label: "terminal",
                    input: durable_input(durable(
                        session_id,
                        2,
                        SessionEventKind::ToolInvocationResultRecorded {
                            record: bcode_session_models::ToolInvocationResultRecord {
                                invocation_id: "call-long-shell".to_owned(),
                                model_output: "complete".to_owned(),
                                content: Vec::new(),
                                is_error: false,
                                presentation: None,
                                result: None,
                            },
                        },
                    )),
                },
                draft(4, 100, r#"{"command":"obsolete"}"#),
                observe(),
            ]);
        for (accepted, idle) in [
            (1, 2),
            (3, 4),
            (5, 6),
            (7, 8),
            (8, 9),
            (10, 11),
            (12, 13),
            (13, 14),
        ] {
            assert_eq!(frames[accepted].text, frames[idle].text);
            assert_eq!(
                frames[accepted].observation.semantic_items,
                frames[idle].observation.semantic_items
            );
        }
        let id = &frames[0].observation.semantic_items[0].0;
        for frame in &frames {
            assert_eq!(frame.observation.semantic_items.len(), 1);
            assert_eq!(&frame.observation.semantic_items[0].0, id);
            assert_eq!(frame.observation.terminal_items.len(), 1);
            assert!(!frame.text.contains("obsolete"));
            assert!(!frame.text.contains("{\"command\""));
        }
        for index in [1, 3, 5, 7, 10] {
            assert!(
                frames[index].text.contains("omega"),
                "{}",
                frames[index].text
            );
        }
    }

    fn assert_shell_acceptance_native_route(app: &BmuxApp) {
        let registry = app
            .plugin_presentation()
            .expect("shell presentation host installed")
            .registry("bcode.shell")
            .expect("static shell registry loaded");
        assert!(
            registry
                .supports_visual_adapter("shell-run-request-card", "bcode.tool.request.shell.run",)
        );
        drop(registry);
        assert!(
            app.plugin_presentation()
                .expect("shell presentation host installed")
                .visual_route("bcode.tool.request.shell.run", 1, Some("bcode.shell"),)
                .is_some(),
            "shell request must have an available native adapter route"
        );
    }

    #[test]
    #[expect(
        clippy::literal_string_with_formatting_args,
        reason = "Python program text is intentional product input, not Rust formatting"
    )]
    fn python_formatter_acceptance_frames_reflow_and_restore_without_source_rewrite() {
        let session_id = SessionId::new();
        let command = "bash -c 'python3 -c \"if ready: data={1:2};print(data)\"'";
        let arguments = serde_json::json!({"command": command, "cwd": "/tmp/project"}).to_string();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(&mut app, 120, 50)
            .with_plugin_host(Arc::new(shell_plugin_host()))
            .run([
                shell_streamed_draft_step(session_id, 1, 1, &arguments),
                TranscriptFrameStep {
                    label: "python-narrow",
                    input: TranscriptFrameInput::Resize(32, 50),
                },
                TranscriptFrameStep {
                    label: "python-idle",
                    input: TranscriptFrameInput::Observe,
                },
                TranscriptFrameStep {
                    label: "python-wide",
                    input: TranscriptFrameInput::Resize(120, 50),
                },
                TranscriptFrameStep {
                    label: "python-request",
                    input: durable_input(durable(
                        session_id,
                        1,
                        SessionEventKind::ToolCallRequested {
                            tool_call_id: "call-long-shell".to_owned(),
                            producer_plugin_id: Some("bcode.shell".to_owned()),
                            tool_name: "shell.run".to_owned(),
                            arguments_json: arguments,
                            working_directory: Some(std::path::PathBuf::from("/tmp/project")),
                        },
                    )),
                },
            ]);
        assert_shell_acceptance_native_route(&app);
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[1].text, frames[2].text);
        assert_eq!(frames[0].text, frames[3].text);
        let identity = &frames[0].observation.semantic_items[0].0;
        for frame in &frames {
            assert_eq!(frame.observation.semantic_items.len(), 1, "{}", frame.text);
            assert_eq!(frame.observation.terminal_items.len(), 1, "{}", frame.text);
            assert_eq!(&frame.observation.semantic_items[0].0, identity);
            assert!(!frame.text.contains("{\"command\""), "{}", frame.text);
        }
        for index in [0, 3, 4] {
            let text = &frames[index].text;
            assert!(text.contains("if ready:"), "{text}");
            assert!(text.contains("    data = {1: 2}"), "{text}");
            assert!(text.contains("    print(data)"), "{text}");
            assert!(!text.contains("data={1:2};print"), "{text}");
        }
    }

    #[test]
    fn nested_command_drafts_reflow_without_losing_lifecycle_identity() {
        let session_id = SessionId::new();
        let command = "bash -c 'if true;then python -c \"print(123); print(456)\"; node -e \"console.log(789)\";fi'";
        let arguments = serde_json::json!({"command": command, "cwd": "/tmp/project"}).to_string();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(&mut app, 100, 50)
            .with_plugin_host(Arc::new(shell_plugin_host()))
            .run([
                shell_streamed_draft_step(
                    session_id,
                    1,
                    1,
                    r#"{"command":"bash -c 'if true;then python -c \"print(123)"#,
                ),
                shell_streamed_draft_step(session_id, 1, 2, &arguments),
                TranscriptFrameStep {
                    label: "narrow-nested",
                    input: TranscriptFrameInput::Resize(32, 50),
                },
                TranscriptFrameStep {
                    label: "idle-nested",
                    input: TranscriptFrameInput::Observe,
                },
                TranscriptFrameStep {
                    label: "final-nested",
                    input: durable_input(durable(
                        session_id,
                        1,
                        SessionEventKind::ToolCallRequested {
                            tool_call_id: "call-long-shell".to_owned(),
                            producer_plugin_id: Some("bcode.shell".to_owned()),
                            tool_name: "shell.run".to_owned(),
                            arguments_json: arguments,
                            working_directory: Some(std::path::PathBuf::from("/tmp/project")),
                        },
                    )),
                },
                TranscriptFrameStep {
                    label: "wide-nested",
                    input: TranscriptFrameInput::Resize(100, 50),
                },
            ]);
        assert_shell_acceptance_native_route(&app);
        assert_eq!(frames.len(), 6);
        for frame in &frames {
            assert_eq!(frame.observation.semantic_items.len(), 1, "{}", frame.text);
            assert_eq!(frame.observation.terminal_items.len(), 1, "{}", frame.text);
            assert!(!frame.text.contains("{\"command\""), "{}", frame.text);
            assert!(!frame.text.contains("Tool · shell.run"), "{}", frame.text);
        }
        for index in [1, 2, 3, 4, 5] {
            assert!(frames[index].text.contains("123"), "{}", frames[index].text);
            assert!(frames[index].text.contains("789"), "{}", frames[index].text);
        }
        assert_eq!(frames[2].text, frames[3].text);
        assert!(!frames[4].text.contains("assembling"), "{}", frames[4].text);
        let identity = &frames[0].observation.semantic_items[0].0;
        assert!(
            frames
                .iter()
                .all(|frame| &frame.observation.semantic_items[0].0 == identity)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Every user-visible shell lifecycle frame is asserted.
    fn shell_draft_request_live_and_result_keep_one_adapter_owned_identity() {
        let session_id = SessionId::new();
        let draft = |revision, preview: &str| TranscriptFrameStep {
            label: if revision == 1 {
                "draft-command"
            } else {
                "draft-cwd"
            },
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-shell".to_owned(),
                        tool_call_id: "call-shell".to_owned(),
                        tool_name: "shell.run".to_owned(),
                        producer_plugin_id: Some("bcode.shell".to_owned()),
                        schema: "bcode.tool.request.shell.run".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation: 1,
                        revision,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: preview.to_owned(),
                        },
                        argument_bytes: preview.len(),
                        truncated: false,
                    },
                },
            }),
        };
        let request = durable(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-shell".to_owned(),
                producer_plugin_id: Some("bcode.shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: serde_json::json!({
                    "command": "printf 'hello from shell\\n'",
                    "cwd": "/tmp/project",
                    "timeout_ms": 30_000,
                    "columns": 100,
                    "rows": 30,
                })
                .to_string(),
                working_directory: Some(std::path::PathBuf::from("/tmp/project")),
            },
        );
        let live_update = |revision: u64, exit_code: Option<i32>| TranscriptFrameStep {
            label: if exit_code.is_some() {
                "terminal-presentation"
            } else {
                "live-output"
            },
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolPresentationUpdated {
                    update: bcode_tool::ToolPresentationUpdate {
                        invocation_id: "call-shell".to_owned(),
                        producer_id: "bcode.shell".to_owned(),
                        generation: 0,
                        revision,
                        identity: bcode_tool::ToolPresentationIdentity::Primary,
                        retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                        schema: "bcode.shell.run".to_owned(),
                        schema_version: 1,
                        artifact: None,
                        payload: serde_json::json!({
                            "mode": "terminal",
                            "timeout_ms": 30_000,
                            "arguments": {
                                "command": "printf 'hello from shell\\n'",
                                "cwd": "/tmp/project",
                                "columns": 100,
                                "rows": 30
                            },
                            "output_tail": "hello from shell\n",
                            "exit_code": exit_code,
                            "columns": 100,
                            "rows": 30
                        }),
                    },
                },
            }),
        };
        let result = durable(
            session_id,
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-shell".to_owned(),
                    model_output: "hello from shell".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "call-shell-shell-run".to_owned(),
                            producer_plugin_id: "bcode.shell".to_owned(),
                            schema: "bcode.shell.run".to_owned(),
                            schema_version: 1,
                            tool_call_id: Some("call-shell".to_owned()),
                            title: Some("Shell run".to_owned()),
                            metadata: serde_json::json!({
                                "mode": "terminal",
                                "timeout_ms": 30_000,
                                "presentation_policy": bcode_config::ShellOutputPresentationPolicy::default(),
                                "format_commands": true,
                                "output_tail": "hello from shell\n",
                                "exit_code": 0,
                                "columns": 100,
                                "rows": 30
                            }),
                            refs: Vec::new(),
                        }),
                    }),
                    content: Vec::new(),
                },
            },
        );

        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .with_plugin_host(Arc::new(shell_plugin_host()))
        .run([
            draft(1, r#"{"command":"printf 'hello"#),
            draft(
                2,
                r#"{"command":"printf 'hello from shell\\n'","cwd":"/tmp/project""#,
            ),
            TranscriptFrameStep {
                label: "assembled-request",
                input: durable_input(request),
            },
            live_update(2, None),
            live_update(3, Some(0)),
            TranscriptFrameStep {
                label: "terminal-result",
                input: durable_input(result),
            },
        ]);

        assert!(
            frames[0].text.contains("printf 'hello"),
            "{}",
            frames[0].text
        );
        assert!(frames[0].text.contains("assembling"), "{}", frames[0].text);
        assert!(
            frames[1].text.contains("/tmp/project"),
            "{}",
            frames[1].text
        );
        assert!(frames[2].text.contains("printf"), "{}", frames[2].text);
        assert!(
            frames[3].text.contains("hello from shell"),
            "{}",
            frames[3].text
        );
        assert!(frames[4].text.contains("exit code 0"), "{}", frames[4].text);
        assert!(frames[5].text.contains("exit code 0"), "{}", frames[5].text);
        let painted_style = |frame: &TranscriptFrameSnapshot, needle: &str| {
            let symbols = needle.chars().map(|ch| ch.to_string()).collect::<Vec<_>>();
            frame
                .cells
                .windows(symbols.len())
                .find(|cells| {
                    cells
                        .iter()
                        .zip(&symbols)
                        .all(|(cell, symbol)| &cell.symbol == symbol)
                })
                .map_or_else(
                    || panic!("missing {needle} in {}: {}", frame.label, frame.text),
                    |cells| cells[0].style,
                )
        };
        let command_style = painted_style(&frames[1], "printf");
        let string_style = painted_style(&frames[1], "'hello");
        assert_ne!(
            command_style.fg, string_style.fg,
            "command and string must retain distinct syntax roles"
        );
        for frame in &frames[2..] {
            assert_eq!(
                painted_style(frame, "printf"),
                command_style,
                "{} lost command highlighting",
                frame.label
            );
            assert_eq!(
                painted_style(frame, "'hello"),
                string_style,
                "{} lost string highlighting",
                frame.label
            );
        }
        assert_no_forbidden_frames(&frames, |frame| {
            (frame.text.contains("Tool · shell.run")
                || frame.text.contains("{\"command\"")
                || frame.observation.semantic_items.len() != 1
                || frame.observation.terminal_items.len() != 1)
                .then(|| "shell lifecycle flashed generic/raw/duplicate content".to_owned())
        })
        .expect("every shell lifecycle frame remains adapter-owned and singular");
        let semantic_id = bcode_session_view_models::TranscriptViewItemId::tool("call-shell");
        assert!(frames.iter().all(|frame| {
            frame.observation.semantic_items[0].0 == semantic_id
                && frame.observation.terminal_items[0].1.as_ref() == Some(&semantic_id)
        }));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle fixture proves timeout and recording identity continuity.
    fn shell_recording_revisions_keep_one_timed_invocation_item() {
        let session_id = SessionId::new();
        let presentation = |revision, committed_bytes| TranscriptFrameStep {
            label: if revision == 1 {
                "recording-first"
            } else {
                "recording-later"
            },
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolPresentationUpdated {
                    update: bcode_tool::ToolPresentationUpdate {
                        invocation_id: "call-shell".to_owned(),
                        producer_id: "bcode.shell".to_owned(),
                        generation: 0,
                        revision,
                        identity: bcode_tool::ToolPresentationIdentity::Primary,
                        retention: bcode_tool::ToolPresentationRetention::RetainLatest,
                        schema: "bcode.shell.run".to_owned(),
                        schema_version: 1,
                        artifact: Some(bcode_tool::ToolContributionArtifact {
                            artifact_id: "call-shell-shell-run".to_owned(),
                            reference_key: "shell_recording".to_owned(),
                            content_type: Some("application/vnd.bcode.shell-recording".to_owned()),
                            storage_uri: "file:///tmp/call-shell.bcsr".to_owned(),
                            committed_bytes,
                            revision: committed_bytes,
                            finalized: false,
                            availability: None,
                        }),
                        payload: serde_json::json!({
                            "mode": "terminal",
                            "timeout_ms": 30_000,
                        }),
                    },
                },
            }),
        };
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .with_plugin_host(Arc::new(shell_plugin_host()))
        .run([
            TranscriptFrameStep {
                label: "accepted",
                input: durable_input(durable(
                    session_id,
                    1,
                    SessionEventKind::ToolCallRequested {
                        tool_call_id: "call-shell".to_owned(),
                        producer_plugin_id: Some("bcode.shell".to_owned()),
                        tool_name: "shell.run".to_owned(),
                        arguments_json: r#"{"command":"printf hello"}"#.to_owned(),
                        working_directory: None,
                    },
                )),
            },
            presentation(1, 64),
            TranscriptFrameStep {
                label: "running",
                input: durable_input(durable(
                    session_id,
                    2,
                    SessionEventKind::ToolInvocationLifecycle {
                        event: bcode_session_models::ToolInvocationLifecycleEvent {
                            invocation_id: "call-shell".to_owned(),
                            sequence: 1,
                            stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                            message: None,
                            metadata: serde_json::Value::Null,
                        },
                    },
                )),
            },
            presentation(2, 128),
        ]);

        assert_eq!(frames.len(), 4);
        for frame in &frames {
            assert_eq!(frame.observation.semantic_items.len(), 1, "{}", frame.text);
            assert_eq!(frame.observation.terminal_items.len(), 1, "{}", frame.text);
            assert_eq!(
                frame.observation.semantic_items[0].0,
                bcode_session_view_models::TranscriptViewItemId::tool("call-shell")
            );
            assert_eq!(
                frame.observation.terminal_items[0].1.as_ref(),
                Some(&frame.observation.semantic_items[0].0)
            );
        }
        assert!(
            frames[1].text.contains("timeout 30.0s"),
            "{}",
            frames[1].text
        );
        assert!(
            frames[2].text.contains("timeout 30.0s"),
            "{}",
            frames[2].text
        );
        assert!(
            frames[3].text.contains("timeout 30.0s"),
            "{}",
            frames[3].text
        );
        assert!(
            frames
                .windows(2)
                .all(|frames| frames[0].observation.terminal_items[0].0
                    == frames[1].observation.terminal_items[0].0)
        );
        assert!(
            frames[3].observation.semantic_items[0].1 > frames[2].observation.semantic_items[0].1
        );
    }

    #[test]
    fn virtual_space_observation_does_not_move_without_new_input() {
        let mut app = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[], false);
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([
            TranscriptFrameStep {
                label: "content",
                input: TranscriptFrameInput::EphemeralPlain("row\n".repeat(30)),
            },
            TranscriptFrameStep {
                label: "space",
                input: TranscriptFrameInput::ScrollDown(4),
            },
            TranscriptFrameStep {
                label: "observe",
                input: TranscriptFrameInput::Observe,
            },
        ]);
        assert_eq!(
            frames[1].observation.viewport_top,
            frames[2].observation.viewport_top
        );
    }

    #[test]
    fn tool_update_preserves_detached_viewport_and_stable_anchor() {
        let session_id = SessionId::new();
        let mut steps = (0..24_u64)
            .map(|index| TranscriptFrameStep {
                label: "history-row",
                input: durable_input(durable(
                    session_id,
                    index.saturating_add(1),
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: format!("history row {index} with enough text to retain an anchor"),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )),
            })
            .collect::<Vec<_>>();
        steps.push(TranscriptFrameStep {
            label: "manual-scroll",
            input: TranscriptFrameInput::ScrollUp(12),
        });
        steps.push(TranscriptFrameStep {
            label: "tool-update",
            input: live_input(SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                    event: bcode_session_models::ToolRequestDraftEvent {
                        output_position: None,
                        turn_id: "turn-1".to_owned(),
                        tool_call_id: "call-write".to_owned(),
                        tool_name: "filesystem.write".to_owned(),
                        producer_plugin_id: Some("bcode.filesystem".to_owned()),
                        schema: "bcode.filesystem.request-draft.write".to_owned(),
                        schema_version: 1,
                        placement: bcode_session_models::ToolContributionPlacement::Request,
                        generation: 1,
                        revision: 1,
                        operation: bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                            start_offset: 0,
                            text: r#"{"path":"src/lib.rs","contents":"hello"}"#.to_owned(),
                        },
                        argument_bytes: 40,
                        truncated: false,
                    },
                },
            }),
        });
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            80,
            12,
        )
        .with_plugin_host(Arc::new(filesystem_plugin_host()))
        .run(steps);
        let detached = &frames[frames.len() - 2].observation;
        let updated = &frames[frames.len() - 1].observation;
        assert_eq!(detached.scroll_mode, "manual_detached");
        assert_eq!(updated.scroll_mode, "manual_detached");
        assert_eq!(updated.viewport_top, detached.viewport_top);
        assert_eq!(updated.anchor, detached.anchor);
        assert!(
            updated.anchor.is_some(),
            "detached viewport lacked stable anchor"
        );
    }

    #[test]
    fn history_prepend_preserves_detached_viewport_and_stable_anchor() {
        let session_id = SessionId::new();
        let mut steps = (0..24_u64)
            .map(|index| TranscriptFrameStep {
                label: "newer-history-row",
                input: durable_input(durable(
                    session_id,
                    index.saturating_add(101),
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: format!("newer row {index} with enough text to retain an anchor"),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                )),
            })
            .collect::<Vec<_>>();
        steps.push(TranscriptFrameStep {
            label: "manual-scroll-before-prepend",
            input: TranscriptFrameInput::ScrollUp(12),
        });
        steps.push(TranscriptFrameStep {
            label: "prepend-older-history",
            input: TranscriptFrameInput::PrependHistory {
                events: (0..8_u64)
                    .map(|index| {
                        durable(
                            session_id,
                            index.saturating_add(1),
                            SessionEventKind::UserMessage {
                                client_id: ClientId::new(),
                                text: format!(
                                    "older row {index} with enough text to shift the document"
                                ),
                                admission: bcode_session_models::TurnAdmissionMetadata::default(),
                            },
                        )
                    })
                    .collect(),
                has_more: false,
            },
        });
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            80,
            12,
        )
        .run(steps);
        let before = &frames[frames.len() - 2].observation;
        let after = &frames[frames.len() - 1].observation;
        assert_eq!(before.scroll_mode, "manual_detached");
        assert_eq!(after.scroll_mode, "manual_detached");
        assert_eq!(after.anchor, before.anchor);
        assert_ne!(after.viewport_top, before.viewport_top);
        assert!(
            after.anchor.is_some(),
            "history prepend lacked stable anchor"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One reverse-arrival scenario covers all positioned output types.
    fn positioned_reasoning_tool_and_assistant_frames_follow_semantic_order() {
        let session_id = SessionId::new();
        let turn_id = "turn-positioned";
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        ).with_plugin_host(Arc::new(filesystem_plugin_host())).run([
            TranscriptFrameStep {
                label: "assistant-position-2",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                        output_position: Some(bcode_session_models::TurnOutputPosition::new(2)),
                        turn_id: turn_id.to_owned(),
                        segment_id: "segment-0".to_owned(),
                        segment_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 1,
                            revision: 1,
                            operation: TextStreamOperation::Append {
                                expected_offset: 0,
                                text: "Final answer".to_owned(),
                            },
                        },
                    },
                }),
            },
            TranscriptFrameStep {
                label: "tool-position-1",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                        event: bcode_session_models::ToolRequestDraftEvent {
                            output_position: Some(bcode_session_models::TurnOutputPosition::new(1)),
                            turn_id: turn_id.to_owned(),
                            tool_call_id: "call-write".to_owned(),
                            tool_name: "filesystem.write".to_owned(),
                            producer_plugin_id: Some("bcode.filesystem".to_owned()),
                            schema: "bcode.filesystem.request-draft.write".to_owned(),
                            schema_version: 1,
                            placement: bcode_session_models::ToolContributionPlacement::Request,
                            generation: 1,
                            revision: 1,
                            operation:
                                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                                    start_offset: 0,
                                    text: r#"{"path":"src/lib.rs","contents":"hello"}"#.to_owned(),
                                },
                            argument_bytes: 40,
                            truncated: false,
                        },
                    },
                }),
            },
            TranscriptFrameStep {
                label: "reasoning-position-0",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                        output_position: Some(bcode_session_models::TurnOutputPosition::new(0)),
                        turn_id: turn_id.to_owned(),
                        activity_id: "reasoning-1".to_owned(),
                        activity_order: 0,
                        part_id: "summary-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        role: bcode_session_models::ReasoningContentRole::Milestone,
                        part_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 1,
                            revision: 1,
                            operation: TextStreamOperation::Append {
                                expected_offset: 0,
                                text: "Plan first".to_owned(),
                            },
                        },
                    },
                }),
            },
        ]);

        let positions = |frame: &TranscriptFrameSnapshot| {
            frame
                .observation
                .semantic_items
                .iter()
                .filter_map(|(id, _)| {
                    // IDs identify the expected positioned semantic rows in shared order.
                    if id.get().contains("reasoning") {
                        Some(0)
                    } else if id.get().contains("tool:") {
                        Some(1)
                    } else if id.get().contains("assistant-turn") {
                        Some(2)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(positions(&frames[0]), vec![2]);
        assert_eq!(positions(&frames[1]), vec![1, 2]);
        assert_eq!(positions(&frames[2]), vec![0, 1, 2]);
        let terminal_rows = &frames[2].observation.terminal_rows;
        let first_row_for = |needle: &str| {
            terminal_rows
                .iter()
                .position(|(id, _)| id.as_ref().is_some_and(|id| id.get().contains(needle)))
                .unwrap_or_else(|| panic!("missing terminal rows for {needle}: {terminal_rows:?}"))
        };
        let reasoning = first_row_for("reasoning");
        let tool = first_row_for("tool:");
        let assistant = first_row_for("assistant-turn");
        assert!(reasoning < tool && tool < assistant, "{terminal_rows:?}");
        assert!(
            frames[2].text.contains("Final answer"),
            "{}",
            frames[2].text
        );
    }

    #[test]
    fn positioned_frames_do_not_cross_system_message_barrier() {
        let session_id = SessionId::new();
        let turn_id = "turn-barrier";
        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "assistant-position-2",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                        output_position: Some(bcode_session_models::TurnOutputPosition::new(2)),
                        turn_id: turn_id.to_owned(),
                        segment_id: "segment-0".to_owned(),
                        segment_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 1,
                            revision: 1,
                            operation: TextStreamOperation::Append {
                                expected_offset: 0,
                                text: "Answer before status".to_owned(),
                            },
                        },
                    },
                }),
            },
            TranscriptFrameStep {
                label: "system-barrier",
                input: durable_input(durable(
                    session_id,
                    1,
                    SessionEventKind::SystemMessage {
                        text: "System status barrier".to_owned(),
                    },
                )),
            },
            TranscriptFrameStep {
                label: "reasoning-position-0",
                input: live_input(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                        output_position: Some(bcode_session_models::TurnOutputPosition::new(0)),
                        turn_id: turn_id.to_owned(),
                        activity_id: "reasoning-1".to_owned(),
                        activity_order: 0,
                        part_id: "summary-0".to_owned(),
                        kind: bcode_session_models::ReasoningContentKind::Summary,
                        role: bcode_session_models::ReasoningContentRole::Milestone,
                        part_order: 0,
                        update: TextStreamUpdate {
                            generation: 0,
                            first_revision: 1,
                            revision: 1,
                            operation: TextStreamOperation::Append {
                                expected_offset: 0,
                                text: "Reasoning after status".to_owned(),
                            },
                        },
                    },
                }),
            },
        ]);

        let items = &frames[2].observation.semantic_items;
        let item_for = |needle: &str| {
            items
                .iter()
                .position(|(id, _)| id.get().contains(needle))
                .unwrap_or_else(|| panic!("missing item containing {needle}: {items:?}"))
        };
        assert!(
            item_for("assistant-turn") < item_for("event:1")
                && item_for("event:1") < item_for("reasoning"),
            "{items:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn screenshot_scale_tool_and_metadata_sequence_has_no_operational_transcript_rows() {
        let session_id = SessionId::new();
        let tools = [
            ("filesystem_read", r#"{"path":"src/lib.rs"}"#),
            ("filesystem_grep", r#"{"pattern":"needle","path":"src"}"#),
            ("filesystem_list", r#"{"path":"src"}"#),
            ("shell", r#"{"command":"cargo check"}"#),
        ];
        let mut sequence = 1_u64;
        let mut steps = Vec::new();
        for (index, (tool_name, arguments_json)) in tools.iter().enumerate() {
            let invocation_id = format!("tool_call_{index}_{tool_name}");
            let work_id = WorkId::new(format!("raw-work-{index}-{tool_name}"));
            steps.push(TranscriptFrameStep {
                label: "tool-request",
                input: durable_input(durable(
                    session_id,
                    sequence,
                    SessionEventKind::ToolCallRequested {
                        tool_call_id: invocation_id.clone(),
                        producer_plugin_id: None,
                        tool_name: (*tool_name).to_owned(),
                        arguments_json: (*arguments_json).to_owned(),
                        working_directory: None,
                    },
                )),
            });
            sequence += 1;
            let metadata = vec![
                durable(
                    session_id,
                    sequence,
                    SessionEventKind::ModelUsage {
                        turn_id: format!("turn-{index}"),
                        usage: SessionTokenUsage {
                            input_tokens: Some(100),
                            output_tokens: Some(10),
                            total_tokens: Some(110),
                            ..SessionTokenUsage::default()
                        },
                    },
                ),
                durable(
                    session_id,
                    sequence + 1,
                    SessionEventKind::RuntimeWorkStarted {
                        work_id: work_id.clone(),
                        kind: RuntimeWorkKind::Tool,
                        label: (*tool_name).to_owned(),
                        tool_call_id: Some(invocation_id.clone()),
                        plugin_id: Some("fixture.plugin".to_owned()),
                        service_interface: None,
                        operation: None,
                        parent_work_id: None,
                        started_at_ms: Some(sequence + 1),
                        cancellable: true,
                    },
                ),
                durable(
                    session_id,
                    sequence + 2,
                    SessionEventKind::RuntimeWorkProgress {
                        work_id: work_id.clone(),
                        message: "halfway".to_owned(),
                        completed_units: Some(1),
                        total_units: Some(2),
                        progress_at_ms: Some(sequence + 2),
                    },
                ),
                durable(
                    session_id,
                    sequence + 3,
                    SessionEventKind::RuntimeWorkFinished {
                        work_id,
                        status: RuntimeWorkStatus::Completed,
                        finished_at_ms: Some(sequence + 3),
                        message: Some("done".to_owned()),
                    },
                ),
            ];
            sequence += 4;
            steps.push(TranscriptFrameStep {
                label: "operational-metadata",
                input: TranscriptFrameInput::DurableBatch(metadata),
            });
            steps.push(TranscriptFrameStep {
                label: "tool-result",
                input: durable_input(durable(
                    session_id,
                    sequence,
                    SessionEventKind::ToolInvocationResultRecorded {
                        record: bcode_session_models::ToolInvocationResultRecord {
                            invocation_id,
                            model_output: format!("{tool_name} complete"),
                            is_error: false,
                            presentation: None,
                            result: None,
                            content: Vec::new(),
                        },
                    },
                )),
            });
            sequence += 1;
        }

        let frames = TranscriptFrameSequence::new(
            &mut BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run(steps);
        assert_no_forbidden_frames(&frames, |frame| {
            [
                "Usage ·",
                "Runtime work",
                "tool_call_",
                "raw-work-",
                "label: filesystem.",
                "label: shell.",
            ]
            .into_iter()
            .find(|forbidden| frame.text.contains(forbidden))
            .map(|forbidden| format!("contains operational metadata {forbidden:?}"))
        })
        .expect("screenshot-scale frames exclude operational metadata");

        for (index, frame) in frames.iter().enumerate() {
            let expected_items = index / 3 + 1;
            assert_eq!(frame.observation.semantic_items.len(), expected_items);
            assert_eq!(frame.observation.terminal_items.len(), expected_items);
            assert_eq!(
                frame.observation.semantic_items.len(),
                frame.observation.terminal_items.len(),
                "frame {index} has duplicate terminal rows"
            );
        }
    }

    #[test]
    fn forbidden_frame_failure_reports_previous_and_failing_frames() {
        let frames = [
            TranscriptFrameSnapshot {
                label: "rich".to_owned(),
                text: "rich request".to_owned(),
                observation: TranscriptFrameObservation {
                    semantic_items: Vec::new(),
                    terminal_items: Vec::new(),
                    terminal_rows: Vec::new(),
                    damage: super::super::transcript_document::TranscriptDocumentDamage::None,
                    viewport_top: 0,
                    scroll_mode: "bottom_follow",
                    anchor: None,
                },
                markdown_fresh_equivalent: true,
                cells: Vec::new(),
            },
            TranscriptFrameSnapshot {
                label: "flash".to_owned(),
                text: "raw json".to_owned(),
                observation: TranscriptFrameObservation {
                    semantic_items: Vec::new(),
                    terminal_items: Vec::new(),
                    terminal_rows: Vec::new(),
                    damage: super::super::transcript_document::TranscriptDocumentDamage::None,
                    viewport_top: 0,
                    scroll_mode: "bottom_follow",
                    anchor: None,
                },
                markdown_fresh_equivalent: true,
                cells: Vec::new(),
            },
        ];
        let error = assert_no_forbidden_frames(&frames, |frame| {
            frame
                .text
                .contains("raw json")
                .then(|| "raw JSON flashed".to_owned())
        })
        .expect_err("forbidden frame must fail");
        let report = error.to_string();
        assert!(report.contains("frame 1 (flash)"), "{report}");
        assert!(report.contains("rich request"), "{report}");
        assert!(report.contains("raw json"), "{report}");
    }
}
