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
    Durable(SessionEvent),
    Live(SessionLiveEvent),
    PrependHistory {
        events: Vec<SessionEvent>,
        has_more: bool,
    },
    DurableBatch(Vec<SessionEvent>),
    Resize(u16, u16),
    ScrollUp(usize),
    AdvanceStreaming(std::time::Duration),
    AssertNoPendingStreaming,
    Observe,
}

pub struct TranscriptFrameStep {
    pub label: &'static str,
    pub input: TranscriptFrameInput,
}

pub struct TranscriptFrameSequence {
    app: BmuxApp,
    width: u16,
    height: u16,
    frames: Vec<TranscriptFrameSnapshot>,
}

impl TranscriptFrameSequence {
    pub const fn new(app: BmuxApp, width: u16, height: u16) -> Self {
        Self {
            app,
            width,
            height,
            frames: Vec::new(),
        }
    }

    pub fn run(
        mut self,
        steps: impl IntoIterator<Item = TranscriptFrameStep>,
    ) -> Vec<TranscriptFrameSnapshot> {
        for step in steps {
            match step.input {
                TranscriptFrameInput::Durable(event) => self.app.absorb_session_event(&event),
                TranscriptFrameInput::Live(event) => self.app.absorb_session_live_event(&event),
                TranscriptFrameInput::PrependHistory { events, has_more } => {
                    self.app.prepend_older_history(&events, has_more);
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
            let options = render::markdown_render_options(&self.app, item, self.width);
            self.app.transcript_markdown_cache().project(item, options);
        }
        let mut buffer = Buffer::empty(Rect::new(0, 0, self.width, self.height));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut self.app, &mut frame);
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
                let options = render::markdown_render_options(&self.app, item, self.width);
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        assert!(!app.apply_presentation_config(bcode_config::PresentationConfig::default()));
        let accepted = "cancelled multi-paragraph output\n\nwith an exact final prefix";
        let frames = TranscriptFrameSequence::new(app, 80, 24).run([
            TranscriptFrameStep {
                label: "stream-start",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
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
                input: TranscriptFrameInput::Durable(durable(
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
        ]);

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
        assert_eq!(frames[1].text, frames[2].text);
    }

    #[test]
    fn smoothed_stream_frame_sequence_preserves_identity_markdown_and_following() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        assert!(!app.apply_presentation_config(bcode_config::PresentationConfig::default()));
        let frames = TranscriptFrameSequence::new(app, 80, 24).run([
            TranscriptFrameStep {
                label: "stream-start",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
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
                input: TranscriptFrameInput::AdvanceStreaming(std::time::Duration::from_millis(
                    100,
                )),
            },
        ]);
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            TranscriptFrameStep {
                label: "initial",
                input: TranscriptFrameInput::Observe,
            },
            TranscriptFrameStep {
                label: "user",
                input: TranscriptFrameInput::Durable(durable(
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
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 100, 40).run(steps);
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let first = format!("# Streaming report\n\n{}", "context row\n\n".repeat(24));
        let second = "- [x] retained projection\n- [ ] final convergence\n\n";
        let final_suffix = "[guide](https://example.com)\n";
        let first_len = first.len();
        let second_offset = first_len.saturating_add(second.len());
        let update = |revision, expected_offset, text: String| {
            TranscriptFrameInput::Live(SessionLiveEvent {
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
        let terminal = TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 72, 14).run([
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let update = |revision, expected_offset, text: &str| {
            TranscriptFrameStep {
            label: if revision == 1 {
                "reasoning-first-part"
            } else {
                "reasoning-second-part"
            },
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let activity_id = "bedrock-messages-reasoning-0";
        let part_id = "raw-0";
        let delta = |label: &'static str, revision: u64, expected_offset: usize, text: &str| {
            TranscriptFrameStep {
                label,
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            delta("thinking-delta-1", 1, 0, "Solving for the ball price"),
            delta("thinking-delta-2", 2, 26, " algebraically"),
            // `content_block_stop` finishes the activity; readable text must remain.
            TranscriptFrameStep {
                label: "reasoning-finished",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
            label: "durable-reasoning-replay",
            input: TranscriptFrameInput::Durable(SessionEvent {
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
            label: "opaque-only-reasoning",
            input: TranscriptFrameInput::Durable(SessionEvent {
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

    /// `/thinking` display modes must change drawn reasoning output, and never blank it silently.
    ///
    /// Summary/raw filtering and `hide` are local presentation choices, so a filtered activity must
    /// say the content is hidden by the display setting rather than render an empty heading.
    #[test]
    fn thinking_display_modes_change_drawn_reasoning_without_blank_frames() {
        let session_id = SessionId::new();
        let reasoning = SessionEvent {
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
        };

        // `all` and `raw` both select raw parts, so the text is drawn.
        for mode in [
            bcode_config::TuiThinkingMode::All,
            bcode_config::TuiThinkingMode::Raw,
        ] {
            let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
            app.set_reasoning_display_mode(mode);
            let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
                label: "reasoning-visible",
                input: TranscriptFrameInput::Durable(reasoning.clone()),
            }]);
            assert!(
                frames[0].text.contains("Raw chain of thought"),
                "{mode:?} must draw raw reasoning: {}",
                frames[0].text
            );
        }

        // `summary` excludes raw parts: the activity must explain the local filter, not go blank.
        let mut summary_app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        summary_app.set_reasoning_display_mode(bcode_config::TuiThinkingMode::Summary);
        let frames =
            TranscriptFrameSequence::new(summary_app, 100, 40).run([TranscriptFrameStep {
                label: "reasoning-filtered-by-summary-mode",
                input: TranscriptFrameInput::Durable(reasoning.clone()),
            }]);
        assert!(
            !frames[0].text.contains("Raw chain of thought"),
            "summary mode must not draw raw reasoning: {}",
            frames[0].text
        );
        assert!(
            frames[0].text.contains("display setting"),
            "filtered reasoning must explain the local display choice: {}",
            frames[0].text
        );

        // `/thinking hide` removes the reasoning item entirely.
        let mut hidden_app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        hidden_app.set_reasoning_visible(false);
        let frames = TranscriptFrameSequence::new(hidden_app, 100, 40).run([TranscriptFrameStep {
            label: "reasoning-hidden",
            input: TranscriptFrameInput::Durable(reasoning),
        }]);
        assert!(
            !frames[0].text.contains("Raw chain of thought"),
            "hidden reasoning must not draw its text: {}",
            frames[0].text
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One transition fixture proves every draft-to-result frame.
    fn filesystem_handoff_has_no_blank_or_raw_json_frame() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(filesystem_plugin_host()));
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
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            TranscriptFrameStep {
                label: "draft-first",
                input: TranscriptFrameInput::Live(draft(1, "hello")),
            },
            TranscriptFrameStep {
                label: "draft-second",
                input: TranscriptFrameInput::Live(draft(2, "hello world")),
            },
            TranscriptFrameStep {
                label: "accepted-request",
                input: TranscriptFrameInput::Durable(request),
            },
            TranscriptFrameStep {
                label: "result",
                input: TranscriptFrameInput::Durable(result),
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.absorb_session_event(&durable(
            session_id,
            1,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call-fast".to_owned(),
                producer_plugin_id: Some("example.plugin".to_owned()),
                tool_name: "example.fast".to_owned(),
                arguments_json: r#"{"transient":"must-not-flash"}"#.to_owned(),
                working_directory: None,
            },
        ));
        app.absorb_session_event(&durable(
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
        ));

        let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
            label: "final-only",
            input: TranscriptFrameInput::Observe,
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(shell_plugin_host()));
        let preview = r#"{"command":"cargo check --workspace","cwd":"/tmp/project"}"#;
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
            label: "coalesced-shell-draft",
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(shell_plugin_host()));
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([TranscriptFrameStep {
            label: "atomic-shell-request",
            input: TranscriptFrameInput::Durable(durable(
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

    #[test]
    #[allow(clippy::too_many_lines)] // Every user-visible shell lifecycle frame is asserted.
    fn shell_draft_request_live_and_result_keep_one_adapter_owned_identity() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(shell_plugin_host()));
        let draft = |revision, preview: &str| TranscriptFrameStep {
            label: if revision == 1 {
                "draft-command"
            } else {
                "draft-cwd"
            },
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                                "arguments": {
                                    "command": "printf 'hello from shell\\n'",
                                    "cwd": "/tmp/project",
                                    "columns": 100,
                                    "rows": 30
                                },
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

        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            draft(1, r#"{"command":"printf 'hello"#),
            draft(
                2,
                r#"{"command":"printf 'hello from shell\\n'","cwd":"/tmp/project""#,
            ),
            TranscriptFrameStep {
                label: "assembled-request",
                input: TranscriptFrameInput::Durable(request),
            },
            live_update(2, None),
            live_update(3, Some(0)),
            TranscriptFrameStep {
                label: "terminal-result",
                input: TranscriptFrameInput::Durable(result),
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(shell_plugin_host()));
        let presentation = |revision, committed_bytes| TranscriptFrameStep {
            label: if revision == 1 {
                "recording-first"
            } else {
                "recording-later"
            },
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            TranscriptFrameStep {
                label: "accepted",
                input: TranscriptFrameInput::Durable(durable(
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
                input: TranscriptFrameInput::Durable(durable(
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
    fn tool_update_preserves_detached_viewport_and_stable_anchor() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(filesystem_plugin_host()));
        let mut steps = (0..24_u64)
            .map(|index| TranscriptFrameStep {
                label: "history-row",
                input: TranscriptFrameInput::Durable(durable(
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
            input: TranscriptFrameInput::Live(SessionLiveEvent {
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
        let frames = TranscriptFrameSequence::new(app, 80, 12).run(steps);
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
        let app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let mut steps = (0..24_u64)
            .map(|index| TranscriptFrameStep {
                label: "newer-history-row",
                input: TranscriptFrameInput::Durable(durable(
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
        let frames = TranscriptFrameSequence::new(app, 80, 12).run(steps);
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_plugin_host(Arc::new(filesystem_plugin_host()));
        let frames = TranscriptFrameSequence::new(app, 100, 40).run([
            TranscriptFrameStep {
                label: "assistant-position-2",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
            BmuxApp::new_with_history(Some(session_id), &[], &[], false),
            100,
            40,
        )
        .run([
            TranscriptFrameStep {
                label: "assistant-position-2",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                input: TranscriptFrameInput::Durable(durable(
                    session_id,
                    1,
                    SessionEventKind::SystemMessage {
                        text: "System status barrier".to_owned(),
                    },
                )),
            },
            TranscriptFrameStep {
                label: "reasoning-position-0",
                input: TranscriptFrameInput::Live(SessionLiveEvent {
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
                input: TranscriptFrameInput::Durable(durable(
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
                input: TranscriptFrameInput::Durable(durable(
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
            BmuxApp::new_with_history(Some(session_id), &[], &[], false),
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
