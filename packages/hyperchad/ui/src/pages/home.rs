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

#[must_use]
pub(super) fn semantic_dom_id(prefix: &str, value: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let hash = value.as_bytes().iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    format!("{prefix}-{hash:016x}-{}", value.len())
}

pub use shell::home;

#[cfg(test)]
mod tests;
