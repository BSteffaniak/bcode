#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Concrete browser-local BMUX adapter over the V2 transcript engine. No daemon, filesystem
//! or event-log access: the transcript is fed bounded canonical history pages and pushed live
//! events, the non-transcript session state arrives as snapshots without transcript items,
//! and the browser retains only that and ephemeral editor, navigation and presentation state.

mod bindings;
pub mod feed;
mod history;
mod image;
mod policy;
mod question;
mod snapshot;
mod surface;
mod transcript;
mod visuals;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
mod wasm;

pub use policy::{
    BrowserBinding, BrowserDelete, BrowserEditorCommand, BrowserInteraction, BrowserMotion,
    BrowserMultiTap, BrowserPolicy, BrowserResolvedBinding, BrowserStroke, BrowserStyle,
    BrowserVisualPolicy, InteractionPlacement, OffscreenFocus,
};
pub use snapshot::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, SnapshotState};
pub use surface::SessionSurface;
#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
pub use wasm::BrowserSession;
