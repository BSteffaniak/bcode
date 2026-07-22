#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Dynamic ABI fixture for the question plugin host tests.
//!
//! The production question plugin can be linked statically with terminal UI support, which must not
//! export dynamic ABI symbols. This fixture wraps the same `QuestionPlugin` implementation as a
//! standalone dynamic library so host tests can exercise dynamic loading without Cargo feature
//! unification with static-bundled consumers.

bcode_plugin_sdk::export_plugin!(
    bcode_question_plugin::QuestionPlugin,
    include_str!("../../../plugins/question-plugin/bcode-plugin.toml")
);
