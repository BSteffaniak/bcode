//! The shared `bcode-git` daemon: one process per user and build serving live repository
//! state ([`bcode_git_live`]) to every client, so watching and git work for a repository
//! happen once however many clients show it.
//!
//! * [`server`]: the daemon. One [`bcode_git_live::Hub`] per repository; a repository is
//!   dropped a while after its last client leaves, and the daemon exits once idle.
//! * [`client`]: [`DaemonSession`] connects (starting the daemon when none runs) and falls
//!   back to serving itself in-process when the daemon goes away.
//! * [`protocol`]: versioned, newline-delimited JSON messages over a local socket.
//! * [`paths`]: where the socket lives. Its name derives from the running executable, so a
//!   client only ever talks to a daemon of its own build, and builds coexist.
//!
//! The daemon needs Unix domain sockets; elsewhere clients serve themselves in-process.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod client;
pub mod paths;
pub mod protocol;
#[cfg(unix)]
pub mod server;

pub use client::{ClientOptions, ConnectError, DaemonSession, Launch};
pub use paths::{default_state_dir, socket_path};
