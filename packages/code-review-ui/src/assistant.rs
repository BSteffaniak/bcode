//! The app-wide review assistant: one docked conversation with a Bcode session, available on
//! the reviews home, in the new-review wizard and in every review.
//!
//! * [`history`] merges the review app's sessions and the review store's bindings into the
//!   conversation history, tagged with where each conversation started.

pub mod history;
