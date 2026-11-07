//! Live git repository state, kept current from filesystem events instead of polling.
//!
//! * [`source`]: where change events come from. [`source::ChangeSource`] is the one
//!   interface every operating-system backend implements.
//! * [`scan`]: how full status scans run. [`scan::StatusScanner`] is plain git or git's
//!   filesystem monitor.
//! * [`Engine`]: turns change events into the narrowest git queries (only the changed paths,
//!   only the index entries that changed, only the files two commits differ in) and keeps
//!   each worktree's status and line counts.
//! * [`Hub`]: runs an engine and a change source on their own thread for any number of
//!   [`Subscriber`]s, coalescing bursts of events so work never overlaps. A daemon runs one
//!   hub per repository for every client; [`LocalSession`] is a hub with one client, and
//!   [`InlineSession`] runs synchronously without watching.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod counts;
mod engine;
mod filter;
pub mod scan;
mod session;
pub mod source;
mod subscriber;
mod worktree;

pub use counts::{CountsMode, LineCounts};
pub use engine::{
    Applied, Engine, EngineConfig, EngineStats, Interest, LiveUpdate, Request, WorktreeSnapshot,
    WorktreeTotal, WorktreeTotalAt,
};
pub use session::{Hub, InlineSession, LiveSession, LocalSession, UpdateSink};
pub use subscriber::Subscriber;
