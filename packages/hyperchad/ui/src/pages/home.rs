//! Home page for the Bcode `HyperChad` application.

mod activity;
mod adapters;
mod composer;
mod interactions;
mod navigation;
mod permissions;
mod shell;
mod tools;
mod transcript;
mod usage;

pub use shell::home;

#[cfg(test)]
mod tests;
