#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shell execution tool plugin for Bcode.
//!
//! This plugin exclusively owns shell/terminal recording schemas, PTY byte capture, replay
//! interpretation, terminal emulation, and shell-result rendering. Host, session, server, and
//! generic TUI-extension code must treat shell recordings as opaque tool artifacts and must not
//! branch on shell schema IDs, recording reference keys, MIME types, ANSI, PTY, resize, grid, or
//! scrollback semantics. Live presentation uses invocation-owned primary replacement updates
//! carrying bounded recording artifact revisions; durable replay uses shell-owned artifact
//! references.
//!
//! Shell execution uses one deadline for command execution, reader startup, capture, and
//! recording finalization. Terminated commands receive a bounded cleanup grace; an unfinished
//! capture returns an explicit error and never supplies a complete replay reference. Unix PTY
//! reads poll nonblocking descriptors so descendants retaining terminal handles cannot postpone
//! the tool result indefinitely. Background jobs must detach stdin as well as stdout/stderr.
//! Recording workers coalesce committed-boundary notices; only the invocation thread publishes
//! host presentation callbacks, keeping output capture independent of presentation throughput.

#[cfg(feature = "native")]
mod contracts;
#[cfg(feature = "presentation")]
pub mod presentation;
#[cfg(feature = "native")]
pub mod recording;
#[cfg(feature = "static-bundled")]
pub mod shell_run_tui;
#[cfg(feature = "native")]
mod terminal_clean;

#[cfg(any(feature = "native", feature = "presentation"))]
mod shell_output;
#[cfg(feature = "presentation")]
mod shell_portable;
#[cfg(feature = "presentation")]
pub use shell_portable::{CommandPresentationPolicy, portable_visual_registrations};
#[cfg(feature = "native")]
include!("native.rs");
