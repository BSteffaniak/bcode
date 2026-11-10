#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// X10/L2: no `static mut` (unusable without unsafe) and, through `clippy.toml`, no thread-locals
// or synchronized globals.
#![forbid(unsafe_code)]

//! Terminal transcript engine ("TUI V2").
//!
//! This crate owns everything between the semantic `SessionView` transcript document and the
//! painted transcript region: the resident history window, units and item layout, the viewport,
//! navigation (the single position authority), transcript focus, the latest-content bar,
//! selection and frame composition. It is embedded by the `bcode_tui` host, which owns I/O; the
//! engine never depends on `bcode_tui`, `bcode_client`, `bcode_ipc`, `bcode_session` or
//! `bcode_server` directly (see `docs/tui-v2-architecture.md`).
//!
//! Modules, one per layer:
//!
//! * [`window`]: resident window manager (paging, eviction, window replacement requests).
//! * [`units`]: list units derived from the transcript document.
//! * [`layout`]: identity-keyed item layout and source positions.
//! * [`viewport`]: controlled BMUX virtual list projection of the navigation position.
//! * [`navigation`]: navigation state, intents and the reducer.
//! * [`focus`]: composer/transcript keyboard focus.
//! * [`markdown`]: Markdown contribution focus and activation.
//! * [`latest_bar`]: latest-content bar decision.
//! * [`selection`]: transcript selection state.
//! * [`frame`]: frame snapshot and clock.
//! * [`sample`]: standalone layout of a fixed item list (previews, generated galleries).
//! * [`engine`]: the façade the host calls.

pub mod engine;
pub mod focus;
pub mod frame;
pub mod latest_bar;
pub mod layout;
pub mod markdown;
pub mod navigation;
pub mod sample;
#[cfg(any(test, feature = "testing"))]
pub mod scenario;
pub mod selection;
pub mod units;
pub mod viewport;
pub mod window;
