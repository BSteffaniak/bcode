//! Plugin-local eval artifact loading and aggregation.

use bcode_eval_models::{
    EvalImprovementCampaign, EvalImprovementGeneration, EvalRepetitionResult, EvalRunResult,
    EvalSuite, EvalVariantRunResult,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Read;
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

/// Loaded eval improvement campaign data for the viewer.
#[derive(Debug, Clone)]
pub struct EvalCampaignData {
    /// Campaign directory.
    pub campaign_dir: PathBuf,
    /// Campaign metadata.
    pub campaign: EvalImprovementCampaign,
    /// Ordered generations.
    pub generations: Vec<EvalImprovementGeneration>,
}

impl EvalCampaignData {
    /// Load an improvement campaign from a campaign directory or campaign JSON path.
    ///
    /// # Errors
    ///
    /// Returns an error when campaign metadata or generation JSON cannot be read.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = path.as_ref();
        let campaign_path = if path.is_dir() {
            path.join("campaign.json")
        } else {
            path.to_path_buf()
        };
        let campaign_dir = campaign_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let mut campaign =
            serde_json::from_str::<EvalImprovementCampaign>(&fs::read_to_string(&campaign_path)?)?;
        campaign.output_dir.clone_from(&campaign_dir);
        let generations_dir = campaign_dir.join("generations");
        let mut generations = Vec::new();
        if let Ok(entries) = fs::read_dir(generations_dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("generation.json");
                if !path.exists() {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(path)
                    && let Ok(generation) = serde_json::from_str::<EvalImprovementGeneration>(&text)
                {
                    generations.push(generation);
                }
            }
        }
        generations.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Self {
            campaign_dir,
            campaign,
            generations,
        })
    }

    /// Load a generation's run data, when it has an associated run.
    #[must_use]
    pub fn generation_run(&self, generation: &EvalImprovementGeneration) -> Option<EvalRunData> {
        generation
            .run_dir
            .as_ref()
            .and_then(|run_dir| EvalRunData::load(run_dir).ok())
    }

    /// Return the parent generation for a generation.
    #[must_use]
    pub fn parent_generation(
        &self,
        generation: &EvalImprovementGeneration,
    ) -> Option<&EvalImprovementGeneration> {
        generation.parent_id.as_ref().and_then(|id| {
            self.generations
                .iter()
                .find(|candidate| &candidate.id == id)
        })
    }
}

/// Discovered eval suite manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalSuiteSummary {
    /// Suite id, when the manifest loaded successfully.
    pub suite_id: String,
    /// Suite manifest path.
    pub suite_path: PathBuf,
    /// Number of configured cases.
    pub cases: usize,
    /// Number of configured variants.
    pub variants: usize,
    /// Load failure, when malformed.
    pub error: Option<String>,
}

/// Discover suite manifests from historical paths and conventional repository roots.
///
/// Discovery is bounded and read-only. At most 2,048 directories and 512 manifests are examined.
#[must_use]
pub fn discover_suites(
    historical_paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<EvalSuiteSummary> {
    const MAX_DIRECTORIES: usize = 2_048;
    const MAX_SUITES: usize = 512;

    let mut paths = historical_paths.into_iter().collect::<BTreeSet<_>>();
    let mut queue = VecDeque::from([PathBuf::from("evals"), PathBuf::from("fixtures/evals")]);
    let mut visited = 0_usize;
    while let Some(directory) = queue.pop_front() {
        if visited >= MAX_DIRECTORIES || paths.len() >= MAX_SUITES {
            break;
        }
        visited = visited.saturating_add(1);
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push_back(path);
            } else if path.file_name().is_some_and(|name| name == "suite.toml") {
                paths.insert(path);
                if paths.len() >= MAX_SUITES {
                    break;
                }
            }
        }
    }

    let mut suites = paths
        .into_iter()
        .map(|suite_path| match bcode_eval::load_suite(&suite_path) {
            Ok(suite) => EvalSuiteSummary {
                suite_id: suite.id,
                suite_path,
                cases: suite.cases.len(),
                variants: suite.variants.len(),
                error: None,
            },
            Err(error) => EvalSuiteSummary {
                suite_id: suite_path.parent().and_then(Path::file_name).map_or_else(
                    || "invalid-suite".to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
                suite_path,
                cases: 0,
                variants: 0,
                error: Some(error.to_string()),
            },
        })
        .collect::<Vec<_>>();
    suites.sort_by(|left, right| {
        left.suite_id
            .cmp(&right.suite_id)
            .then_with(|| left.suite_path.cmp(&right.suite_path))
    });
    suites
}

/// Aggregate for one case in one campaign generation.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalCaseHistoryCell {
    /// Generation id.
    pub generation_id: String,
    /// Average pass rate across variants containing the case.
    pub pass_rate: f64,
    /// Average judge score across repetitions when available.
    pub score: Option<f64>,
    /// Repetition count.
    pub repetitions: usize,
}

/// Campaign history for one case.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalCaseHistoryRow {
    /// Case id.
    pub case_id: String,
    /// Generation cells in campaign order.
    pub cells: Vec<EvalCaseHistoryCell>,
}

/// Build a campaign-wide case history matrix.
#[must_use]
pub fn campaign_case_history(data: &EvalCampaignData) -> Vec<EvalCaseHistoryRow> {
    let mut rows = BTreeMap::<String, Vec<EvalCaseHistoryCell>>::new();
    for generation in &data.generations {
        let Some(run) = data.generation_run(generation) else {
            continue;
        };
        let mut cases = BTreeMap::<String, (f64, usize, f64, usize, usize)>::new();
        for variant in &run.result.variants {
            for case in &variant.cases {
                let entry = cases.entry(case.case_id.clone()).or_default();
                entry.0 += case.pass_rate;
                entry.1 = entry.1.saturating_add(1);
                for repetition in &case.repetitions {
                    entry.4 = entry.4.saturating_add(1);
                    for judge in &repetition.judges {
                        if let Some(score) = judge.score {
                            entry.2 += score;
                            entry.3 = entry.3.saturating_add(1);
                        }
                    }
                }
            }
        }
        for (case_id, (pass_total, variants, score_total, scores, repetitions)) in cases {
            rows.entry(case_id).or_default().push(EvalCaseHistoryCell {
                generation_id: generation.id.clone(),
                pass_rate: pass_total / len_as_f64(variants),
                score: (scores > 0).then(|| score_total / len_as_f64(scores)),
                repetitions,
            });
        }
    }
    rows.into_iter()
        .map(|(case_id, cells)| EvalCaseHistoryRow { case_id, cells })
        .collect()
}

/// Return all measurement names present across campaign generation runs.
#[must_use]
pub fn campaign_metric_names(data: &EvalCampaignData) -> Vec<String> {
    let mut names = BTreeSet::new();
    for generation in &data.generations {
        if let Some(run) = data.generation_run(generation) {
            for repetition in run.repetitions() {
                names.extend(repetition.measurements.keys().cloned());
            }
        }
    }
    names.into_iter().collect()
}

/// Campaign metadata for picker rows.
#[derive(Debug, Clone)]
pub struct EvalCampaignSummary {
    /// Campaign directory.
    pub campaign_dir: PathBuf,
    /// Campaign id.
    pub campaign_id: String,
    /// Suite id.
    pub suite_id: String,
    /// Generation count.
    pub generations: usize,
    /// Best generation id.
    pub best_generation_id: Option<String>,
    /// Latest generation id.
    pub latest_generation_id: Option<String>,
    /// Directory modification time in milliseconds since Unix epoch, when available.
    pub modified_ms: u128,
}

/// Discover improvement campaigns below a root directory.
#[must_use]
pub fn discover_campaigns(root: impl AsRef<Path>) -> Vec<EvalCampaignSummary> {
    let root = root.as_ref();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut campaigns = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !path.join("campaign.json").exists() {
            continue;
        }
        let Ok(data) = EvalCampaignData::load(&path) else {
            continue;
        };
        let modified_ms = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis());
        campaigns.push(EvalCampaignSummary {
            campaign_dir: path,
            campaign_id: data.campaign.id,
            suite_id: data.campaign.suite_id,
            generations: data.generations.len(),
            best_generation_id: data.campaign.best_generation_id,
            latest_generation_id: data.campaign.latest_generation_id,
            modified_ms,
        });
    }
    campaigns.sort_by_key(|campaign| std::cmp::Reverse(campaign.modified_ms));
    campaigns
}

/// Average pass rate across variants.
#[must_use]
pub fn run_pass_rate(result: &EvalRunResult) -> f64 {
    if result.variants.is_empty() {
        return 0.0;
    }
    let variant_count =
        u32::try_from(result.variants.len()).map_or_else(|_| f64::from(u32::MAX), f64::from);
    result
        .variants
        .iter()
        .map(|variant| variant.pass_rate)
        .sum::<f64>()
        / variant_count
}

/// Best overall score across variants.
#[must_use]
pub fn run_best_score(result: &EvalRunResult) -> f64 {
    result
        .variants
        .iter()
        .map(|variant| variant.score.overall)
        .fold(0.0_f64, f64::max)
}

/// Average variant measurement value for a run.
#[must_use]
pub fn run_avg_measurement(result: &EvalRunResult, metric: &str) -> Option<f64> {
    let values = result
        .variants
        .iter()
        .filter_map(|variant| variant.measurements.get(metric).copied())
        .collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        let value_count =
            u32::try_from(values.len()).map_or_else(|_| f64::from(u32::MAX), f64::from);
        Some(values.iter().sum::<f64>() / value_count)
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
    /// Whether the source exceeded the read limit.
    pub truncated: bool,
    /// Whether the source appears to be binary.
    pub binary: bool,
}

const MAX_ARTIFACT_BYTES: u64 = 1_048_576;

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
    let relative = if artifact.path.is_absolute() {
        return None;
    } else if artifact.path.components().count() > 1 {
        artifact.path.clone()
    } else {
        PathBuf::from("cases")
            .join(&repetition.case_id)
            .join("variants")
            .join(&repetition.variant_id)
            .join("repetitions")
            .join(format!("{:04}", repetition.repetition))
            .join(&artifact.path)
    };
    let canonical_root = run_dir.canonicalize().ok()?;
    let path = canonical_root.join(relative).canonicalize().ok()?;
    if !path.starts_with(&canonical_root) || !path.is_file() {
        return None;
    }
    let metadata = fs::metadata(&path).ok()?;
    let truncated = metadata.len() > MAX_ARTIFACT_BYTES;
    let file = fs::File::open(&path).ok()?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, MAX_ARTIFACT_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let binary = bytes.contains(&0);
    let text = if binary {
        format!("Binary artifact ({} bytes)", metadata.len())
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Some(TextArtifact {
        title: format!("{}: {}", kind, path.display()),
        text,
        truncated,
        binary,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_eval_models::EvalArtifactRef;

    fn repetition(path: PathBuf) -> EvalRepetitionResult {
        EvalRepetitionResult {
            case_id: "case".to_string(),
            variant_id: "variant".to_string(),
            repetition: 1,
            passed: true,
            exit_code: Some(0),
            wall_time_ms: 1,
            measurements: BTreeMap::new(),
            judges: Vec::new(),
            diagnostics: Vec::new(),
            artifacts: vec![EvalArtifactRef {
                kind: "diff".to_string(),
                path,
            }],
        }
    }

    #[test]
    fn artifact_loader_rejects_paths_outside_run_root() {
        let root = std::env::temp_dir().join(format!("bcode-eval-artifact-{}", std::process::id()));
        let outside = root.with_extension("outside");
        fs::create_dir_all(&root).expect("create run root");
        fs::write(&outside, "secret").expect("write outside file");
        let result = load_repetition_artifact(
            &root,
            &repetition(PathBuf::from("../").join(outside.file_name().expect("file name"))),
            "diff",
        );
        assert!(result.is_none());
        let _ = fs::remove_file(outside);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn artifact_loader_detects_binary_content() {
        let root =
            std::env::temp_dir().join(format!("bcode-eval-binary-artifact-{}", std::process::id()));
        let path = root
            .join("cases/case/variants/variant/repetitions/0001")
            .join("artifact.bin");
        fs::create_dir_all(path.parent().expect("artifact parent")).expect("create run root");
        fs::write(&path, [0, 1, 2]).expect("write binary file");
        let artifact =
            load_repetition_artifact(&root, &repetition(PathBuf::from("artifact.bin")), "diff")
                .expect("load binary artifact");
        assert!(artifact.binary);
        assert!(!artifact.truncated);
        let _ = fs::remove_dir_all(root);
    }
}
