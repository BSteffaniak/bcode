#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Independent, local-first dictation engine and platform adapters.
pub mod audio;
pub mod cli;
pub mod config;
pub mod contracts;
pub mod delivery;
pub mod models;
pub mod recognition;
pub mod runtime;
pub mod service;
pub mod shortcuts;
pub mod speech;
pub mod text;
pub use contracts::{Error, Result};
