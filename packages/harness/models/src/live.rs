//! Transient live text sidecar and the single stream-identity scheme shared with durable text.
//!
//! Live updates are presentation hints only: they are never persisted, never part of the
//! observation digest, and are superseded by the durable observation with the same identity.
use crate::{HarnessContractError, MAX_HARNESS_LIVE_UPDATES, id, text};
use serde::{Deserialize, Serialize};

/// Which assistant stream a transient live update extends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessLiveStream {
    /// Assistant display text, finalized by `HarnessObservationKind::Text`.
    Text,
    /// Readable reasoning text, finalized by `HarnessObservationKind::Reasoning`.
    Reasoning,
}

/// One contiguous transient append to a native assistant segment.
///
/// `offset` is the UTF-8 byte offset of `text` within the segment's accumulated live text.
/// Consumers must reject non-contiguous appends rather than guessing. The text is untrusted
/// display content and grants no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessLiveUpdate {
    pub stream: HarnessLiveStream,
    pub segment_id: String,
    pub offset: u64,
    pub text: String,
}
impl HarnessLiveUpdate {
    /// Validate bounded identity and text.
    /// # Errors
    /// Rejects malformed segment identities and excessive text.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.segment_id)?;
        text(&self.text)
    }
}

/// Validate a page's live sidecar bounds.
/// # Errors
/// Rejects excessive update counts or malformed updates.
pub fn validate_live_updates(updates: &[HarnessLiveUpdate]) -> Result<(), HarnessContractError> {
    if updates.len() > MAX_HARNESS_LIVE_UPDATES {
        return Err(HarnessContractError::BoundsExceeded);
    }
    updates.iter().try_for_each(HarnessLiveUpdate::validate)
}

/// Session item keys for one native assistant segment.
///
/// Compatible with `SessionLiveEventKind::AssistantTextStreamUpdated` (`turn_id`,
/// `segment_id`, `segment_order`) and `AssistantReasoningTextStreamUpdated` (`turn_id`,
/// `activity_id`, `activity_order`, `part_id`, `part_order`), and with the session-view item
/// identifiers `assistant-turn:{turn}:segment:{segment}` and `reasoning-turn:{turn}:{activity}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HarnessStreamIdentity {
    /// Synthetic per-segment turn, scoped by ownership generation.
    pub turn_id: String,
    /// Native segment identity, unchanged.
    pub segment_id: String,
    /// Always zero: each native segment is its own synthetic turn.
    pub segment_order: u32,
}
impl HarnessStreamIdentity {
    /// Session-view assistant item identifier for this segment.
    #[must_use]
    pub fn assistant_item_id(&self) -> String {
        format!(
            "assistant-turn:{}:segment:{}",
            self.turn_id, self.segment_id
        )
    }
    /// Reasoning activity identifier (equal to the segment identity).
    #[must_use]
    pub fn reasoning_activity_id(&self) -> &str {
        &self.segment_id
    }
    /// Reasoning part identifier (equal to the segment identity).
    #[must_use]
    pub fn reasoning_part_id(&self) -> &str {
        &self.segment_id
    }
    /// Session-view reasoning item identifier for this segment.
    #[must_use]
    pub fn reasoning_item_id(&self) -> String {
        format!("reasoning-turn:{}:{}", self.turn_id, self.segment_id)
    }
}

/// Prefix shared by every [`harness_stream_identity`] turn id of one ownership generation.
///
/// Lets a consumer retire every stream of a generation (for example when its native turn ends
/// or its transport is gone) without knowing individual segment identities.
#[must_use]
pub fn harness_stream_turn_prefix(generation: u64) -> String {
    format!("harness:{generation}:")
}

/// Single source of truth mapping a native segment to durable and live session item keys.
///
/// `turn_id` is `harness:{generation}:{segment_id}`, so a new ownership generation never
/// collides with or reopens items from an earlier one.
#[must_use]
pub fn harness_stream_identity(generation: u64, segment_id: &str) -> HarnessStreamIdentity {
    HarnessStreamIdentity {
        turn_id: format!("{}{segment_id}", harness_stream_turn_prefix(generation)),
        segment_id: segment_id.to_owned(),
        segment_order: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_generation_scoped_and_matches_item_schemes() {
        let first = harness_stream_identity(1, "msg-1:0");
        let second = harness_stream_identity(2, "msg-1:0");
        assert_ne!(first.turn_id, second.turn_id);
        assert!(first.turn_id.starts_with(&harness_stream_turn_prefix(1)));
        assert!(!second.turn_id.starts_with(&harness_stream_turn_prefix(1)));
        assert_eq!(first, harness_stream_identity(1, "msg-1:0"));
        assert_eq!(
            first.assistant_item_id(),
            "assistant-turn:harness:1:msg-1:0:segment:msg-1:0"
        );
        assert_eq!(
            first.reasoning_item_id(),
            "reasoning-turn:harness:1:msg-1:0:msg-1:0"
        );
        assert_eq!(first.segment_order, 0);
    }

    #[test]
    fn live_updates_are_bounded() {
        let update = HarnessLiveUpdate {
            stream: HarnessLiveStream::Text,
            segment_id: "segment".into(),
            offset: 0,
            text: "hello".into(),
        };
        assert!(validate_live_updates(std::slice::from_ref(&update)).is_ok());
        assert!(
            validate_live_updates(&vec![update.clone(); MAX_HARNESS_LIVE_UPDATES + 1]).is_err()
        );
        let mut invalid = update;
        invalid.segment_id = " padded".into();
        assert!(invalid.validate().is_err());
    }
}
