//! Packaged launcher for the shared BMUX Mermaid worker.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

fn main() {
    bcode_mermaid_render::worker_main();
}
