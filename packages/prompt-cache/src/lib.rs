#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Prompt-cache semantics for Bcode model requests.
//!
//! This crate owns how Bcode expects a model's prompt cache to behave and whether an observed
//! request sequence matched that expectation:
//!
//! * [`expectations`] derives [`PromptCacheExpectations`] from normalized provider and model
//!   capability claims.
//! * [`planning`] places host-side cache breakpoints for a request from those same claims.
//! * [`analysis`] turns normalized usage into measurements and verdicts; it is the only place
//!   cache-ratio arithmetic lives.
//! * [`scenarios`] drives a live or in-process provider through a fixed set of cache workloads
//!   using the public provider operations.
//! * [`simulation`] (feature `simulation`) is a deterministic reference cache that test providers
//!   use so the planner, scenarios, and analyzer can be verified without credentials.
//!
//! Provider wire formats stay in provider plugins; this crate consumes only normalized
//! [`bcode_model`] types.

pub mod analysis;
pub mod expectations;
pub mod planning;
pub mod scenarios;
#[cfg(feature = "simulation")]
pub mod simulation;

pub use bcode_prompt_cache_models::{
    CacheRoundObservation, CacheWriteTtlTokens, PROMPT_CACHE_VERIFICATION_REPORT_SCHEMA_VERSION,
    PromptCacheExpectations, PromptCacheMechanism, PromptCacheScenarioOutcome,
    PromptCacheScenarioResult, PromptCacheThresholds, PromptCacheVerificationReport, measurement,
};

/// Conservative minimum cacheable prefix, in tokens, used when no claim declares one.
///
/// Every major provider caches prefixes of at least this length; treating shorter prefixes as
/// cacheable would place breakpoints that can never be reused.
pub const DEFAULT_MIN_PREFIX_TOKENS: u64 = 1_024;

/// Estimate tokens from a serialized character count using Bcode's shared 4-chars-per-token rule.
///
/// This mirrors the host request estimator so planning decisions and workload sizing agree with
/// context accounting.
#[must_use]
pub fn estimated_tokens_from_chars(chars: usize) -> u64 {
    u64::try_from(chars).unwrap_or(u64::MAX).saturating_add(3) / 4
}
