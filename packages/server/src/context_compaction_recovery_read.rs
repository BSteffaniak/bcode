//! Explicit recovery reads are paginated independently of operation age. These bounds apply
//! only to maintenance preparation/admission, never ordinary history or model context.

use super::*;

pub(in crate::context_compaction) const RECOVERY_MAX_EVENTS: usize = 16_384;
const RECOVERY_PAGE_EVENTS: usize = 128;
const RECOVERY_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Read a complete bounded canonical range. Partial pages are never evidence of settlement.
pub(in crate::context_compaction) async fn recovery_events(
    state: &ServerState,
    session_id: SessionId,
    start: u64,
    end: u64,
) -> Result<Vec<bcode_session_models::SessionEvent>, CompactionError> {
    let count = end
        .checked_sub(start)
        .and_then(|n| n.checked_add(1))
        .filter(|n| *n <= RECOVERY_MAX_EVENTS as u64)
        .ok_or_else(invalid_checkpoint)?;
    let mut events = Vec::new();
    let mut cursor = start;
    let mut bytes = 0_usize;
    while cursor <= end {
        let page_end = end.min(cursor.saturating_add(RECOVERY_PAGE_EVENTS as u64 - 1));
        let budget = bcode_session::persisted::EventDecodeBudget::new(
            RECOVERY_MAX_BYTES.saturating_sub(bytes),
            bcode_session_models::retention::ObservationRetentionLimits {
                scalar_capacity_bytes: RECOVERY_MAX_BYTES,
                collection_entries: 1_048_576,
                value_nodes: 1_048_576,
                depth: 128,
                copies: RECOVERY_PAGE_EVENTS,
            },
        );
        let (page, _) = state
            .sessions
            .session_history_page_with_decode_budget(
                session_id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: Some(bcode_session_models::SessionHistoryCursor { sequence: cursor }),
                    limit: usize::try_from(page_end - cursor + 1)
                        .map_err(|_| invalid_checkpoint())?,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                },
                budget,
            )
            .await
            .map_err(|_| invalid_checkpoint())?;
        if !page.compatibility_issues.is_empty() {
            return Err(invalid_checkpoint());
        }
        let page = page.events;
        if page.len() as u64 != page_end - cursor + 1
            || page.iter().enumerate().any(|(offset, event)| {
                event.session_id != session_id || event.sequence != cursor + offset as u64
            })
        {
            return Err(invalid_checkpoint());
        }
        for event in &page {
            bytes = bytes
                .checked_add(
                    serde_json::to_vec(event)
                        .map_err(|_| invalid_checkpoint())?
                        .len(),
                )
                .ok_or_else(invalid_checkpoint)?;
            if bytes > RECOVERY_MAX_BYTES {
                return Err(invalid_checkpoint());
            }
        }
        events.extend(page);
        if page_end == end {
            break;
        }
        cursor = page_end + 1;
    }
    if events.len() as u64 != count {
        return Err(invalid_checkpoint());
    }
    Ok(events)
}
