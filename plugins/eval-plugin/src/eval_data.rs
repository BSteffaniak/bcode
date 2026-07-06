//! Plugin-local eval artifact loading and aggregation.

use bcode_eval_models::{EvalRepetitionResult, EvalRunResult, EvalSuite, EvalVariantRunResult};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Loaded eval run data for the viewer.
#[derive(Debug, Clone)]
pub struct EvalRunData {
    /// Run directory.
    pub run_dir: PathBuf,
    /// Run result.
    pub result: EvalRunResult,
    /// Suite snapshot, when present.
    pub suite: Option<EvalSuite>,
}

impl EvalRunData {
    /// Load an eval run from a run directory or summary JSON path.
    ///
    /// # Errors
    ///
    /// Returns an error when the summary cannot be read or decoded.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = path.as_ref();
        let summary_path = if path.is_dir() {
            path.join("summary.json")
        } else {
            path.to_path_buf()
        };
        let run_dir = summary_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let result = serde_json::from_str::<EvalRunResult>(&fs::read_to_string(&summary_path)?)?;
        let suite = fs::read_to_string(run_dir.join("suite.snapshot.toml"))
            .ok()
            .and_then(|text| toml::from_str::<EvalSuite>(&text).ok());
        Ok(Self {
            run_dir,
            result,
            suite,
        })
    }

    /// Return flattened repetitions.
    #[must_use]
    pub fn repetitions(&self) -> Vec<&EvalRepetitionResult> {
        self.result
            .variants
            .iter()
            .flat_map(|variant| variant.cases.iter())
            .flat_map(|case| case.repetitions.iter())
            .collect()
    }

    /// Return all tool metric names.
    #[must_use]
    pub fn tool_metric_names(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for repetition in self.repetitions() {
            for key in repetition.measurements.keys() {
                if key.starts_with("tool_call_count.") {
                    names.insert(key.clone());
                }
            }
        }
        names.into_iter().collect()
    }
}

/// Run metadata for picker rows.
#[derive(Debug, Clone)]
pub struct EvalRunSummary {
    /// Run directory.
    pub run_dir: PathBuf,
    /// Run id.
    pub run_id: String,
    /// Suite id.
    pub suite_id: String,
    /// Whether the run passed.
    pub passed: bool,
    /// Variant count.
    pub variants: usize,
    /// Best variant id.
    pub winner: Option<String>,
    /// Directory modification time in milliseconds since Unix epoch, when available.
    pub modified_ms: u128,
}

/// Discover eval runs below a root directory.
#[must_use]
pub fn discover_runs(root: impl AsRef<Path>) -> Vec<EvalRunSummary> {
    let root = root.as_ref();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut runs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(data) = EvalRunData::load(&path) else {
            continue;
        };
        let modified_ms = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis());
        runs.push(EvalRunSummary {
            run_dir: path,
            run_id: data.result.manifest.run_id.clone(),
            suite_id: data.result.manifest.suite_id.clone(),
            passed: data.result.passed,
            variants: data.result.variants.len(),
            winner: best_variant(&data.result).map(|variant| variant.variant_id.clone()),
            modified_ms,
        });
    }
    runs.sort_by(|left, right| {
        right
            .modified_ms
            .cmp(&left.modified_ms)
            .then_with(|| right.run_id.cmp(&left.run_id))
    });
    runs
}

/// Metrics for a variant.
#[derive(Debug, Clone, Default)]
pub struct VariantMetrics {
    /// Repetition count.
    pub repetitions: usize,
    /// Average wall time in milliseconds.
    pub avg_wall_ms: f64,
    /// Total tokens.
    pub total_tokens: f64,
    /// Average tokens per repetition.
    pub avg_tokens: f64,
    /// Total tool calls.
    pub tool_calls: f64,
    /// Average tool calls per repetition.
    pub avg_tool_calls: f64,
    /// Total tool errors.
    pub tool_errors: f64,
    /// Permission prompts.
    pub permission_prompts: f64,
}

/// Aggregate metrics for one variant.
#[must_use]
pub fn variant_metrics(variant: &EvalVariantRunResult) -> VariantMetrics {
    let repetitions = variant_repetitions(variant);
    let count = len_as_f64(repetitions.len().max(1));
    let total_tokens = sum_refs_metric(&repetitions, "total_tokens");
    let tool_calls = sum_refs_metric(&repetitions, "tool_call_count");
    VariantMetrics {
        repetitions: repetitions.len(),
        avg_wall_ms: sum_refs_metric(&repetitions, "wall_time_ms") / count,
        total_tokens,
        avg_tokens: total_tokens / count,
        tool_calls,
        avg_tool_calls: tool_calls / count,
        tool_errors: sum_refs_metric(&repetitions, "tool_error_count"),
        permission_prompts: sum_refs_metric(&repetitions, "permission_prompt_count"),
    }
}

/// Return total metric for a variant.
#[must_use]
pub fn sum_variant_metric(variant: &EvalVariantRunResult, metric: &str) -> f64 {
    variant
        .cases
        .iter()
        .flat_map(|case| case.repetitions.iter())
        .filter_map(|repetition| repetition.measurements.get(metric).copied())
        .sum()
}

/// Return a per-case average metric.
#[must_use]
pub fn case_avg_metric(repetitions: &[EvalRepetitionResult], metric: &str) -> f64 {
    if repetitions.is_empty() {
        return 0.0;
    }
    repetitions
        .iter()
        .filter_map(|repetition| repetition.measurements.get(metric).copied())
        .sum::<f64>()
        / len_as_f64(repetitions.len())
}

fn len_as_f64(len: usize) -> f64 {
    f64::from(u32::try_from(len).unwrap_or(u32::MAX))
}

/// Format a compact number.
#[must_use]
pub fn format_number(value: f64) -> String {
    if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if value.abs() < 0.5 {
        "0".to_string()
    } else {
        format!("{value:.0}")
    }
}

/// Format milliseconds.
#[must_use]
pub fn format_duration_ms(value: f64) -> String {
    if value >= 1_000.0 {
        format!("{:.1}s", value / 1_000.0)
    } else {
        format!("{value:.0}ms")
    }
}

/// Best variant by score.
#[must_use]
pub fn best_variant(result: &EvalRunResult) -> Option<&EvalVariantRunResult> {
    result.variants.iter().max_by(|left, right| {
        left.score
            .overall
            .partial_cmp(&right.score.overall)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn variant_repetitions(variant: &EvalVariantRunResult) -> Vec<&EvalRepetitionResult> {
    variant
        .cases
        .iter()
        .flat_map(|case| case.repetitions.iter())
        .collect()
}

fn sum_refs_metric(repetitions: &[&EvalRepetitionResult], metric: &str) -> f64 {
    repetitions
        .iter()
        .filter_map(|repetition| repetition.measurements.get(metric).copied())
        .sum()
}

/// Count distinct diff artifacts for repetitions.
#[must_use]
pub fn diff_variant_count(run_dir: &Path, repetitions: &[EvalRepetitionResult]) -> usize {
    let mut hashes = BTreeSet::new();
    for repetition in repetitions {
        for artifact in &repetition.artifacts {
            if artifact.kind == "diff" {
                let path = if artifact.path.is_absolute() {
                    artifact.path.clone()
                } else {
                    run_dir.join(&artifact.path)
                };
                if let Ok(text) = fs::read_to_string(path) {
                    hashes.insert(stable_text_hash(&text));
                }
            }
        }
    }
    hashes.len()
}

fn stable_text_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One loaded text artifact.
#[derive(Debug, Clone, Default)]
pub struct TextArtifact {
    /// Display title.
    pub title: String,
    /// Text body.
    pub text: String,
}

/// Load a repetition artifact by kind.
#[must_use]
pub fn load_repetition_artifact(
    run_dir: &Path,
    repetition: &EvalRepetitionResult,
    kind: &str,
) -> Option<TextArtifact> {
    let artifact = repetition
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == kind)?;
    let path = if artifact.path.is_absolute() {
        artifact.path.clone()
    } else if artifact.path.components().count() > 1 {
        run_dir.join(&artifact.path)
    } else {
        run_dir
            .join("cases")
            .join(&repetition.case_id)
            .join("variants")
            .join(&repetition.variant_id)
            .join("repetitions")
            .join(format!("{:04}", repetition.repetition))
            .join(&artifact.path)
    };
    let text = fs::read_to_string(&path).ok()?;
    Some(TextArtifact {
        title: format!("{}: {}", kind, path.display()),
        text,
    })
}

/// Return metric map totals grouped by key.
#[must_use]
pub fn measurement_totals(data: &EvalRunData) -> BTreeMap<String, f64> {
    let mut totals = BTreeMap::new();
    for repetition in data.repetitions() {
        for (key, value) in &repetition.measurements {
            *totals.entry(key.clone()).or_default() += value;
        }
    }
    totals
}
