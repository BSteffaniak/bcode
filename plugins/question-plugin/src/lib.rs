#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Interactive question tool plugin for Bcode.

#[cfg(any(feature = "native", feature = "presentation"))]
pub mod question_controller;
mod question_domain;
pub mod question_interaction;
#[cfg(feature = "native")]
mod question_native;
#[cfg(feature = "presentation")]
mod question_outcome;
#[cfg(feature = "presentation")]
pub use question_outcome::portable_visual_registrations;
#[cfg(feature = "static-bundled")]
mod question_outcome_tui;
#[cfg(feature = "presentation")]
mod question_tui;
mod question_types;
pub use question_domain::{
    answered_outcome, dismissed_outcome, format_question_outcome_output, parse_question_request,
    unanswered_outcome,
};
#[cfg(feature = "native")]
pub use question_native::{QuestionPlugin, static_plugin};
#[cfg(feature = "static-bundled")]
pub use question_native::{
    question_interaction_adapter, question_interaction_registry, question_tui_registry,
};
pub use question_types::*;
#[cfg(feature = "native")]
const TOOL_NAME: &str = "question";
#[cfg(any(feature = "native", feature = "presentation"))]
const QUESTION_EXCHANGE_SCHEMA: &str = "bcode.question.request";
const MAX_QUESTIONS: usize = 32;
const MAX_OPTIONS_PER_QUESTION: usize = 100;
const MAX_QUESTION_TEXT_BYTES: usize = 16 * 1024;
const MAX_OPTION_TEXT_BYTES: usize = 4 * 1024;
const MAX_CUSTOM_ANSWER_BYTES: usize = 64 * 1024;

/// Shared plugin-owned BMUX question form for native and Canvas hosts.
#[cfg(feature = "presentation")]
pub mod presentation;
