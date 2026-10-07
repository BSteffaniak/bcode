//! Independent, opt-in judgement benchmarks over bounded public-consumer captures.
//!
//! No provider execution, session reads, persistence, or application authority lives here.
//! Scorer calibration is not live quality evidence. Repeats are clustered by task; verdicts
//! are preregistered descriptive gates, not significance or population generalization.

mod comparison;
mod contracts;
mod corpus;
mod diagnostics;
pub use diagnostics::{ArmDiagnostics, ClassMetrics, RankingMetrics};
mod vocabulary;
pub use vocabulary::validate_service_oracle_vocabulary;
mod scoring;
mod uncertainty;

pub use corpus::parse_corpus_plan;
pub use uncertainty::BOOTSTRAP_SEED;

pub use comparison::compare_benchmark;
mod quality;
pub use contracts::*;
pub use quality::{QualityBenchmarkReport, QualityComparisonScope, compare_benchmark_quality_only};
pub use scoring::score_output;

#[cfg(test)]
mod audit_tests;
#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod v2_tests;
#[cfg(test)]
mod v3_tests;
