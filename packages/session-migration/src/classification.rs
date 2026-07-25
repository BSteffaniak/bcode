//! Deterministic treatment assigned to decoded historical session events.

use bcode_session_models::SessionEvent;

/// Source identity retained for one converted historical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalEventMetadata {
    /// Durable event schema that produced the source payload.
    pub source_schema: u16,
    /// Durable event-kind name in the source payload.
    pub source_kind: String,
}

/// Migration classification for one recognized historical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoricalDecode {
    /// The payload already uses the current durable representation.
    Current(SessionEvent),
    /// Historical semantics were converted into an active current event.
    Converted {
        /// Current event produced from the historical payload.
        event: SessionEvent,
        /// Source schema and kind used for audit accounting.
        metadata: HistoricalEventMetadata,
    },
    /// Historical semantics are recognized but intentionally inert now.
    RetiredKnown {
        /// Current inert event retaining the historical payload.
        event: SessionEvent,
        /// Source schema and kind used for audit accounting.
        metadata: HistoricalEventMetadata,
    },
}

impl HistoricalDecode {
    /// Return the current event materialized by this classification.
    #[must_use]
    pub const fn event(&self) -> &SessionEvent {
        match self {
            Self::Current(event)
            | Self::Converted { event, .. }
            | Self::RetiredKnown { event, .. } => event,
        }
    }

    /// Return historical source metadata when conversion or retirement occurred.
    #[must_use]
    pub const fn metadata(&self) -> Option<&HistoricalEventMetadata> {
        match self {
            Self::Current(_) => None,
            Self::Converted { metadata, .. } | Self::RetiredKnown { metadata, .. } => {
                Some(metadata)
            }
        }
    }

    /// Return whether this event was recognized as retired inert history.
    #[must_use]
    pub const fn is_retired_known(&self) -> bool {
        matches!(self, Self::RetiredKnown { .. })
    }

    /// Consume the classification and return its current event.
    #[must_use]
    pub fn into_event(self) -> SessionEvent {
        match self {
            Self::Current(event)
            | Self::Converted { event, .. }
            | Self::RetiredKnown { event, .. } => event,
        }
    }
}
