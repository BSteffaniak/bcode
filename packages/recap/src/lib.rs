#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Independent video understanding, visual recap composition, and portable export.
pub mod analysis_contracts;
pub mod composition;
pub mod contracts;
pub mod detailed;
pub mod diagnostics;
pub mod engine;
pub mod export;
pub mod media;
pub mod model;
pub mod process;
pub mod source;
pub mod source_analysis;
pub mod storage;
