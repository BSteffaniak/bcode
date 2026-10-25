#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Concrete browser-local BMUX adapter. No daemon, filesystem or event-log access.
//! The semantic document is authoritative; the browser retains only full accepted
//! snapshots and ephemeral editor, navigation and presentation state.

mod bindings;
mod snapshot;
mod surface;
mod transcript;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
mod wasm;

pub use snapshot::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, SnapshotState};
pub use surface::SessionSurface;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
pub use wasm::BrowserSession;
