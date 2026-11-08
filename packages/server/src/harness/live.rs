//! Transient native live text publication.
//!
//! A committed observation page may carry a live sidecar of contiguous appends to native
//! assistant segments. The daemon republishes those appends as ordinary session live stream
//! events keyed by the shared harness stream identity, so renderers treat them exactly like
//! native Bcode streaming and replace them in place when the durable segment commits.
//!
//! Live text is untrusted display content. It is never persisted and grants no authority.

use bcode_harness_models::{
    HarnessLiveStream, HarnessLiveUpdate, HarnessObservation, HarnessObservationKind,
    harness_stream_identity,
};
use bcode_session_models::{
    ReasoningContentKind, ReasoningContentRole, SessionLiveEventKind, TextStreamOperation,
    TextStreamUpdate,
};
use std::collections::BTreeSet;

/// Live streams use one fixed stream generation. The harness ownership generation is already
/// part of the stream identity, so a recovered generation never extends an earlier stream.
const LIVE_STREAM_GENERATION: u64 = 1;

/// Segments finalized by durable observations in the same page.
fn durable_segments(observations: &[HarnessObservation]) -> BTreeSet<(HarnessLiveStream, &str)> {
    observations
        .iter()
        .filter_map(|observation| match &observation.kind {
            HarnessObservationKind::Text {
                segment_id: Some(segment),
                ..
            } => Some((HarnessLiveStream::Text, segment.as_str())),
            HarnessObservationKind::Reasoning {
                segment_id: Some(segment),
                ..
            } => Some((HarnessLiveStream::Reasoning, segment.as_str())),
            _ => None,
        })
        .collect()
}

/// Map one contiguous live append onto a session live stream operation.
///
/// Revisions are byte positions: an append of `n` bytes at offset `o` represents revisions
/// `o + 1 ..= o + n`. Contiguous appends therefore have contiguous revisions regardless of
/// which page or daemon published them, and a gap or overlap is detected by every consumer.
/// Empty appends carry no revision and are skipped.
fn stream_update(update: &HarnessLiveUpdate) -> Option<TextStreamUpdate> {
    let length = u64::try_from(update.text.len()).ok()?;
    if length == 0 {
        return None;
    }
    let first_revision = update.offset.checked_add(1)?;
    let revision = update.offset.checked_add(length)?;
    Some(TextStreamUpdate {
        generation: LIVE_STREAM_GENERATION,
        first_revision,
        revision,
        operation: TextStreamOperation::Append {
            expected_offset: usize::try_from(update.offset).ok()?,
            text: update.text.clone(),
        },
    })
}

/// Build the session live events for one committed page's live sidecar.
///
/// Updates for segments that the same page finalizes durably are dropped: the durable segment
/// supersedes live presentation, and publishing after it would reopen finalized text.
#[must_use]
pub(super) fn live_events(
    generation: u64,
    observations: &[HarnessObservation],
    live: &[HarnessLiveUpdate],
) -> Vec<SessionLiveEventKind> {
    let finalized = durable_segments(observations);
    live.iter()
        .filter(|update| update.validate().is_ok())
        .filter(|update| !finalized.contains(&(update.stream, update.segment_id.as_str())))
        .filter_map(|update| {
            let stream = stream_update(update)?;
            let identity = harness_stream_identity(generation, &update.segment_id);
            Some(match update.stream {
                HarnessLiveStream::Text => SessionLiveEventKind::AssistantTextStreamUpdated {
                    output_position: None,
                    turn_id: identity.turn_id,
                    segment_id: identity.segment_id,
                    segment_order: identity.segment_order,
                    update: stream,
                },
                HarnessLiveStream::Reasoning => {
                    SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                        output_position: None,
                        activity_id: identity.reasoning_activity_id().to_owned(),
                        part_id: identity.reasoning_part_id().to_owned(),
                        turn_id: identity.turn_id,
                        activity_order: identity.segment_order,
                        // Matches the durable SessionView projection so display filtering agrees.
                        kind: ReasoningContentKind::Legacy,
                        role: ReasoningContentRole::Unknown,
                        part_order: 0,
                        update: stream,
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(
        stream: HarnessLiveStream,
        segment: &str,
        offset: u64,
        text: &str,
    ) -> HarnessLiveUpdate {
        HarnessLiveUpdate {
            stream,
            segment_id: segment.into(),
            offset,
            text: text.into(),
        }
    }

    fn text_observation(segment: &str) -> HarnessObservation {
        HarnessObservation {
            observation_id: format!("text-{segment}"),
            session_id: "session".into(),
            generation: 3,
            native_correlations: Vec::new(),
            completeness: bcode_harness_models::HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Text {
                text: "done".into(),
                segment_id: Some(segment.into()),
            },
        }
    }

    fn stream_of(event: &SessionLiveEventKind) -> (&str, &TextStreamUpdate) {
        match event {
            SessionLiveEventKind::AssistantTextStreamUpdated {
                turn_id, update, ..
            }
            | SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                turn_id, update, ..
            } => (turn_id, update),
            _ => panic!("not a stream update"),
        }
    }

    #[test]
    fn contiguous_appends_have_contiguous_revisions_across_pages() {
        let first = live_events(
            3,
            &[],
            &[update(HarnessLiveStream::Text, "msg:0", 0, "héllo")],
        );
        let second = live_events(
            3,
            &[],
            &[update(HarnessLiveStream::Text, "msg:0", 6, " world")],
        );
        let (turn, first) = stream_of(&first[0]);
        assert_eq!(turn, harness_stream_identity(3, "msg:0").turn_id);
        let (_, second) = stream_of(&second[0]);
        assert_eq!(first.first_revision, 1);
        assert_eq!(second.first_revision, first.revision + 1);
        assert!(matches!(
            &second.operation,
            TextStreamOperation::Append {
                expected_offset: 6,
                ..
            }
        ));
    }

    #[test]
    fn durable_segment_in_same_page_suppresses_live_and_empty_appends_are_skipped() {
        let events = live_events(
            3,
            &[text_observation("done:0")],
            &[
                update(HarnessLiveStream::Text, "done:0", 0, "stale"),
                update(HarnessLiveStream::Reasoning, "done:0", 0, "thinking"),
                update(HarnessLiveStream::Text, "open:0", 0, ""),
            ],
        );
        assert_eq!(events.len(), 1, "only the reasoning stream remains open");
        assert!(matches!(
            &events[0],
            SessionLiveEventKind::AssistantReasoningTextStreamUpdated { activity_id, part_id, .. }
                if activity_id == "done:0" && part_id == "done:0"
        ));
    }

    #[test]
    fn generation_scopes_stream_identity() {
        let previous = live_events(1, &[], &[update(HarnessLiveStream::Text, "s", 0, "a")]);
        let recovered = live_events(2, &[], &[update(HarnessLiveStream::Text, "s", 0, "a")]);
        assert_ne!(stream_of(&previous[0]).0, stream_of(&recovered[0]).0);
    }
}
