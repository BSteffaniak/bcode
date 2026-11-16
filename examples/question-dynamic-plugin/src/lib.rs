#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Thin shared-ABI dynamic packaging for the production question definition.
//! No lifecycle, dispatch, manifest or UI behavior is copied into this target.

bmux_plugin_sdk::export_plugin_entrypoint!(bcode_question_plugin::plugin_definition);
