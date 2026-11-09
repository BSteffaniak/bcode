use super::*;
use serde_json::json;

fn start(buffer: &mut LiveBuffer, id: &str) {
    buffer.stream_event(&json!({"type":"stream_event","event":{
        "type":"message_start","message":{"id":id}}}));
}
fn block(buffer: &mut LiveBuffer, index: u64, kind: &str) {
    buffer.stream_event(&json!({"type":"stream_event","event":{
        "type":"content_block_start","index":index,"content_block":{"type":kind}}}));
}
fn delta(buffer: &mut LiveBuffer, index: u64, kind: &str, field: &str, text: &str) {
    buffer.stream_event(&json!({"type":"stream_event","event":{
        "type":"content_block_delta","index":index,"delta":{"type":kind, (field):text}}}));
}

#[test]
fn deltas_coalesce_with_running_offsets_and_segment_identity() {
    let mut buffer = LiveBuffer::default();
    start(&mut buffer, "msg_1");
    block(&mut buffer, 0, "thinking");
    delta(&mut buffer, 0, "thinking_delta", "thinking", "Let me ");
    delta(&mut buffer, 0, "thinking_delta", "thinking", "think");
    delta(
        &mut buffer,
        0,
        "signature_delta",
        "signature",
        "SECRET_SIGNATURE",
    );
    block(&mut buffer, 1, "text");
    delta(&mut buffer, 1, "text_delta", "text", "Hé");
    delta(&mut buffer, 1, "text_delta", "text", "llo");
    assert!(buffer.is_pending());
    let updates = buffer.take();
    assert!(!buffer.is_pending());
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].stream, HarnessLiveStream::Reasoning);
    assert_eq!(updates[0].segment_id, "msg_1:r0");
    assert_eq!(
        (updates[0].offset, updates[0].text.as_str()),
        (0, "Let me think")
    );
    assert_eq!(updates[1].stream, HarnessLiveStream::Text);
    assert_eq!(updates[1].segment_id, "msg_1:t0");
    assert_eq!((updates[1].offset, updates[1].text.as_str()), (0, "Héllo"));
    assert!(bcode_harness_models::validate_live_updates(&updates).is_ok());
    // Offered updates are not repeated; later appends continue at the byte offset.
    delta(&mut buffer, 1, "text_delta", "text", "!");
    let next = buffer.take();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].offset, "Héllo".len() as u64);
    assert!(!serde_json::to_string(&next).unwrap().contains("SECRET"));
}

#[test]
fn unstarted_blocks_subagents_and_tool_input_are_not_presented() {
    let mut buffer = LiveBuffer::default();
    // No message identity yet.
    block(&mut buffer, 0, "text");
    delta(&mut buffer, 0, "text_delta", "text", "orphan");
    start(&mut buffer, "msg_2");
    // No block start for index 3.
    delta(&mut buffer, 3, "text_delta", "text", "unknown offset");
    block(&mut buffer, 1, "tool_use");
    delta(
        &mut buffer,
        1,
        "input_json_delta",
        "partial_json",
        "{\"command\"",
    );
    buffer.stream_event(
        &json!({"type":"stream_event","parent_tool_use_id":"toolu_1",
        "event":{"type":"message_start","message":{"id":"child"}}}),
    );
    block(&mut buffer, 0, "text");
    // A thinking delta for a text block is ignored rather than mislabelled.
    delta(&mut buffer, 0, "thinking_delta", "thinking", "mismatch");
    assert!(!buffer.is_pending());
    delta(&mut buffer, 0, "text_delta", "text", "ok");
    let updates = buffer.take();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].segment_id, "msg_2:t0");
}

#[test]
fn pressure_drops_updates_and_suppresses_noncontiguous_appends() {
    let mut buffer = LiveBuffer::default();
    start(&mut buffer, "msg_3");
    for index in 0..u64::try_from(MAX_HARNESS_LIVE_UPDATES + 4).unwrap() {
        block(&mut buffer, index % 60, "text");
        delta(&mut buffer, index % 60, "text_delta", "text", "x");
    }
    let large = "y".repeat(MAX_UPDATE_TEXT * 2 + 3);
    block(&mut buffer, 61, "text");
    delta(&mut buffer, 61, "text_delta", "text", &large);
    let updates = buffer.take();
    assert!(updates.len() <= MAX_HARNESS_LIVE_UPDATES);
    assert!(
        updates
            .iter()
            .all(|update| update.text.len() <= MAX_UPDATE_TEXT)
    );
    assert!(bcode_harness_models::validate_live_updates(&updates).is_ok());
    // Block 61 overflowed: further appends stay suppressed instead of leaving a hole.
    delta(&mut buffer, 61, "text_delta", "text", "tail");
    assert!(
        buffer
            .take()
            .iter()
            .all(|update| update.segment_id != "msg_3:61")
    );
    // A new block after pressure streams normally from offset zero.
    block(&mut buffer, 62, "text");
    delta(&mut buffer, 62, "text_delta", "text", "fresh");
    let fresh = buffer.take();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].offset, 0);
}

#[test]
fn large_single_delta_splits_into_contiguous_bounded_updates() {
    let mut buffer = LiveBuffer::default();
    start(&mut buffer, "msg_4");
    block(&mut buffer, 0, "text");
    let text = "é".repeat(MAX_UPDATE_TEXT);
    delta(&mut buffer, 0, "text_delta", "text", &text);
    let updates = buffer.take();
    assert!(updates.len() >= 2);
    let mut offset = 0_u64;
    let mut joined = String::new();
    for update in &updates {
        assert_eq!(update.offset, offset);
        offset += update.text.len() as u64;
        joined.push_str(&update.text);
    }
    assert_eq!(joined, text);
    buffer.clear();
    delta(&mut buffer, 0, "text_delta", "text", "after clear");
    assert!(!buffer.is_pending());
}

#[test]
fn near_capacity_reports_before_updates_would_be_dropped() {
    let mut buffer = LiveBuffer::default();
    start(&mut buffer, "msg_5");
    block(&mut buffer, 0, "text");
    delta(&mut buffer, 0, "text_delta", "text", "small");
    assert!(!buffer.near_capacity(), "a few deltas keep coalescing");
    // Contiguous deltas share updates; half the byte bound stops coalescing early.
    let chunk = "z".repeat(MAX_UPDATE_TEXT);
    while !buffer.near_capacity() {
        delta(&mut buffer, 0, "text_delta", "text", &chunk);
    }
    let updates = buffer.take();
    let mut offset = 0_u64;
    for update in &updates {
        assert_eq!(update.offset, offset, "nothing was dropped before capacity");
        offset += update.text.len() as u64;
    }
    assert!(
        !buffer.near_capacity(),
        "offering the page frees the window"
    );
}

#[test]
fn text_after_an_unfinalized_thinking_block_keeps_the_durable_segment() {
    // Natively, an empty thinking block streams but yields no durable reasoning frame; the
    // following text must still be the message's first text segment on both sides.
    let mut buffer = LiveBuffer::default();
    start(&mut buffer, "msg_4");
    block(&mut buffer, 0, "thinking");
    block(&mut buffer, 1, "text");
    delta(&mut buffer, 1, "text_delta", "text", "Final answer");
    let updates = buffer.take();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].segment_id, "msg_4:t0");
    let mut numbering = crate::normalize::display::BlockNumbering::new();
    assert_eq!(
        numbering.next("msg_4", crate::normalize::display::SegmentKind::Text),
        updates[0].segment_id
    );
}
