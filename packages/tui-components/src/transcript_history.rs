//! Pagination port for renderer-local transcript navigation.
//!
//! This is a TUI boundary, not a public application/frontend contract. The native
//! adapter owns history cursors; navigation only observes availability and reveal
//! correspondence. Browser adapters use the same state machine without IPC.

/// History-window operations needed by the transcript viewport.
pub trait TranscriptHistory {
    /// Whether rows exist before the resident window.
    fn has_older_history(&self) -> bool;
    /// Whether rows exist after the resident window.
    fn has_newer_history(&self) -> bool;
    /// Outstanding movement toward older rows.
    fn reveal_request(&self) -> Option<usize>;
    /// Outstanding movement toward newer rows.
    fn newer_reveal_request(&self) -> Option<usize>;
    /// Cancel unfulfilled movement toward newer rows.
    fn cancel_newer_reveal(&mut self);
    /// Cancel unfulfilled movement toward older rows.
    fn cancel_older_reveal(&mut self);
    /// Request older rows, retaining unfulfilled movement.
    fn request_load(&mut self, rows: usize);
    /// Request newer rows, retaining unfulfilled movement.
    fn request_load_newer(&mut self, rows: usize);
    /// Clear navigation requests after explicitly following the tail.
    fn clear_reveal_request(&mut self);
    /// Consume page acceptance once: older ready, newer ready, newer has content.
    fn take_ready_pages(&mut self) -> (bool, bool, bool);
    /// Consume movement proven by correspondence with a surviving source item.
    fn consume_reveal(&mut self, older: bool, rows: usize);
}
