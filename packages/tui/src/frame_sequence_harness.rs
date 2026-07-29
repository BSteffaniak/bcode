//! Deterministic transcript frame-sequence acceptance harness.

use super::app::{BmuxApp, TranscriptFrameObservation};
use super::render;
use bcode_session_models::{SessionEvent, SessionLiveEvent};
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptFrameSnapshot {
    pub label: String,
    pub text: String,
    pub observation: TranscriptFrameObservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptFrameSequenceError {
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

pub(crate) fn assert_no_forbidden_frames(
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

pub(crate) enum TranscriptFrameInput {
    Durable(SessionEvent),
    Live(SessionLiveEvent),
    Observe,
}

pub(crate) struct TranscriptFrameStep {
    pub label: &'static str,
    pub input: TranscriptFrameInput,
}

pub(crate) struct TranscriptFrameSequence {
    app: BmuxApp,
    width: u16,
    height: u16,
    frames: Vec<TranscriptFrameSnapshot>,
}

impl TranscriptFrameSequence {
    pub(crate) const fn new(app: BmuxApp, width: u16, height: u16) -> Self {
        Self {
            app,
            width,
            height,
            frames: Vec::new(),
        }
    }

    pub(crate) fn run(
        mut self,
        steps: impl IntoIterator<Item = TranscriptFrameStep>,
    ) -> Vec<TranscriptFrameSnapshot> {
        for step in steps {
            match step.input {
                TranscriptFrameInput::Durable(event) => self.app.absorb_session_event(&event),
                TranscriptFrameInput::Live(event) => self.app.absorb_session_live_event(&event),
                TranscriptFrameInput::Observe => {}
            }
            let frame = self.capture(step.label);
            self.frames.push(frame);
        }
        self.frames
    }

    fn capture(&mut self, label: &str) -> TranscriptFrameSnapshot {
        let mut buffer = Buffer::empty(Rect::new(0, 0, self.width, self.height));
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut self.app, &mut frame);
        let text = (0..buffer.area().height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        TranscriptFrameSnapshot {
            label: label.to_owned(),
            text,
            observation: self.app.frame_observation(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ClientId, SessionEventKind, SessionId, TextStreamOperation, TextStreamUpdate,
    };

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
