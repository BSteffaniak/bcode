#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! UI components and templates for the bcode documentation website.

pub mod doc_pages;
pub mod home_layout;
pub mod pages;
