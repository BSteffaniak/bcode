//! Deterministic guide planning: one agent or fan-out, and how a fan-out partitions the review.
//!
//! With `strategy = "auto"`:
//!
//! * A review within `single_max_files`, `single_max_lines` and two layers gets one agent.
//! * A stack stepped through three or more layers gets one child per layer.
//! * Anything else gets area children: changed files clustered by package root (the nearest
//!   directory with a `Cargo.toml`, `package.json` or `pyproject.toml`), else by top directory,
//!   balanced by changed lines into at most `max_children` areas.
//!
//! `depth` adjusts what the guide covers and whether a verifier pass runs.

use super::config::{GuideConfig, StrategyChoice, VerifyMode};
use bcode_code_review_models::{GuideDepth, GuideStrategy};
use serde::Serialize;
use std::collections::BTreeMap;

/// Bytes of diff counted per changed line, for estimates.
pub const BYTES_PER_CHANGED_LINE: u64 = 60;
/// Tokens a child agent spends beyond its share of the diff (instructions, tools, writing).
pub const CHILD_OVERHEAD_TOKENS: u64 = 30_000;
/// Tokens one agent spends beyond the diff (instructions, tools, writing).
pub const AGENT_OVERHEAD_TOKENS: u64 = 20_000;
/// Directory levels below a package root tried when one cluster holds everything.
const MAX_CLUSTER_DEPTH: usize = 3;

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    /// Path.
    pub path: String,
    /// Its source.
    pub source_id: String,
    /// Added and removed lines.
    pub changed_lines: u64,
}

/// One stepped stack layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLayer {
    /// Its source id.
    pub source_id: String,
    /// Its label (`#102`).
    pub label: String,
}

/// What a plan is made from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanInput {
    /// The review's changed (or shown) files.
    pub files: Vec<PlanFile>,
    /// Stack layers stepped through one at a time, bottom first; empty otherwise.
    pub layers: Vec<PlanLayer>,
    /// Directories holding a package manifest (`""` for the repository root).
    pub package_roots: Vec<String>,
    /// Whether the review shows files without a diff.
    pub without_diff: bool,
    /// Bytes of resolved context.
    pub context_bytes: usize,
}

/// Size of the review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanMetrics {
    /// Files.
    pub files: usize,
    /// Added and removed lines.
    pub changed_lines: u64,
    /// Stepped stack layers (1 when not stepped).
    pub layers: usize,
    /// Packages the files are in.
    pub packages: usize,
    /// Bytes of resolved context.
    pub context_bytes: usize,
}

/// One area of a fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanArea {
    /// Area id.
    pub area_id: String,
    /// Title.
    pub title: String,
    /// `layer` or `paths`.
    pub kind: &'static str,
    /// The layer's source id, for a layer area.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Directories or files the area covers, for a paths area.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Files in it.
    pub files: usize,
    /// Changed lines in it.
    pub changed_lines: u64,
}

/// What a cost estimate is made from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanEstimate {
    /// Approximate diff bytes (changed lines × [`BYTES_PER_CHANGED_LINE`]).
    pub diff_bytes: u64,
    /// Approximate input tokens of the diff and context (bytes / 4).
    pub input_tokens: u64,
    /// Depth factor, in tenths (brief 10, standard 15, deep 25).
    pub depth_factor_tenths: u64,
    /// Child agents.
    pub children: usize,
    /// Estimated tokens in all.
    pub estimated_tokens: u64,
    /// Whether a fan-out asks first (`confirm_above_tokens`).
    pub needs_confirmation: bool,
    /// Soft token cap of a fan-out.
    pub budget_tokens: u64,
}

/// A plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GuidePlan {
    /// One agent, per layer, or per area.
    pub strategy: GuideStrategy,
    /// Why.
    pub reason: String,
    /// The depth planned.
    pub depth: GuideDepth,
    /// Whether a verifier pass runs.
    pub verifier: bool,
    /// Size of the review.
    pub metrics: PlanMetrics,
    /// Fan-out areas; empty for one agent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<PlanArea>,
    /// Concurrency of a fan-out.
    pub max_parallel: usize,
    /// Cost estimate inputs.
    pub estimate: PlanEstimate,
    /// What the depth asks of the guide.
    pub guidance: Vec<String>,
}

/// Plan a guide for `input` under `config`; `strategy` and `depth` override the configuration.
#[must_use]
pub fn plan(
    input: &PlanInput,
    config: &GuideConfig,
    strategy: Option<StrategyChoice>,
    depth: Option<GuideDepth>,
) -> GuidePlan {
    let depth = depth.unwrap_or(config.depth);
    let changed_lines: u64 = input.files.iter().map(|file| file.changed_lines).sum();
    let layers = input.layers.len().max(1);
    let metrics = PlanMetrics {
        files: input.files.len(),
        changed_lines,
        layers,
        packages: input
            .files
            .iter()
            .filter_map(|file| package_of(&file.path, &input.package_roots))
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        context_bytes: input.context_bytes,
    };
    let (strategy, reason) = choose(
        input,
        config,
        strategy.unwrap_or(config.strategy),
        changed_lines,
    );
    let areas = match strategy {
        GuideStrategy::Single => Vec::new(),
        GuideStrategy::PerLayer => layer_areas(input),
        GuideStrategy::Areas => {
            let mut grouped = clusters(&input.files, &input.package_roots, 0);
            let mut level = 0;
            while grouped.len() == 1 && level < MAX_CLUSTER_DEPTH {
                level += 1;
                let deeper = clusters(&input.files, &input.package_roots, level);
                if deeper.len() > 1 {
                    grouped = deeper;
                }
            }
            balance(grouped, config.max_children)
        }
    };
    let verifier = match config.verify {
        VerifyMode::Never => false,
        VerifyMode::Always => true,
        VerifyMode::Deep => depth == GuideDepth::Deep,
    };
    let diff_bytes = changed_lines.saturating_mul(BYTES_PER_CHANGED_LINE);
    let input_tokens = (diff_bytes + u64::try_from(input.context_bytes).unwrap_or(u64::MAX)) / 4;
    let depth_factor_tenths = match depth {
        GuideDepth::Brief => 10,
        GuideDepth::Standard => 15,
        GuideDepth::Deep => 25,
    };
    let children = areas.len() + usize::from(verifier);
    let estimated_tokens = input_tokens.saturating_mul(depth_factor_tenths) / 10
        + AGENT_OVERHEAD_TOKENS
        + u64::try_from(children)
            .unwrap_or(u64::MAX)
            .saturating_mul(CHILD_OVERHEAD_TOKENS);
    GuidePlan {
        strategy,
        reason,
        depth,
        verifier,
        metrics,
        max_parallel: config.max_parallel,
        estimate: PlanEstimate {
            diff_bytes,
            input_tokens,
            depth_factor_tenths,
            children,
            estimated_tokens,
            needs_confirmation: !areas.is_empty() && estimated_tokens > config.confirm_above_tokens,
            budget_tokens: config.budget_tokens,
        },
        areas,
        guidance: guidance(depth, input.without_diff),
    }
}

/// The strategy `choice` comes to for `input`, and why.
fn choose(
    input: &PlanInput,
    config: &GuideConfig,
    choice: StrategyChoice,
    changed_lines: u64,
) -> (GuideStrategy, String) {
    let layers = input.layers.len().max(1);
    let small = input.files.len() <= config.single_max_files
        && changed_lines <= config.single_max_lines
        && layers <= 2;
    match choice {
        StrategyChoice::Single => (GuideStrategy::Single, "one agent, as asked".to_owned()),
        StrategyChoice::FanOut if input.layers.len() >= 2 => (
            GuideStrategy::PerLayer,
            "one child per layer, as asked".to_owned(),
        ),
        StrategyChoice::FanOut => (GuideStrategy::Areas, "area children, as asked".to_owned()),
        StrategyChoice::Auto if small => (
            GuideStrategy::Single,
            format!(
                "{} files and {changed_lines} changed lines in {layers} layer(s) fit one agent (at most {} files, {} lines, 2 layers)",
                input.files.len(),
                config.single_max_files,
                config.single_max_lines
            ),
        ),
        StrategyChoice::Auto if input.layers.len() >= 3 => (
            GuideStrategy::PerLayer,
            format!(
                "a stack stepped through {} layers: one child writes each layer's guide, then the stack guide links them",
                input.layers.len()
            ),
        ),
        StrategyChoice::Auto => (
            GuideStrategy::Areas,
            format!(
                "{} files and {changed_lines} changed lines are more than one agent covers well; area children each write one part",
                input.files.len()
            ),
        ),
    }
}

fn guidance(depth: GuideDepth, without_diff: bool) -> Vec<String> {
    let mut lines = vec![
        "Write the core sections: purpose, scope, and a reading order whose steps each cite the lines they walk through.".to_owned(),
    ];
    if without_diff {
        lines.push(
            "The review shows files without a diff: explain the code (purpose, architecture, entry points, invariants) rather than changes."
                .to_owned(),
        );
    }
    match depth {
        GuideDepth::Brief => lines.push(
            "Brief: core sections only; read nothing beyond the review's files.".to_owned(),
        ),
        GuideDepth::Standard => lines.push(
            "Standard: add adaptive sections (risks, behavior changes, ...) only with cited evidence; read context outside the diff only when it matters."
                .to_owned(),
        ),
        GuideDepth::Deep => lines.push(
            "Deep: more steps; read callers, implementations and tests (code_review.related_files, code_review.search_code) and cite them; a verifier checks each claim."
                .to_owned(),
        ),
    }
    lines
}

fn layer_areas(input: &PlanInput) -> Vec<PlanArea> {
    input
        .layers
        .iter()
        .map(|layer| {
            let files: Vec<&PlanFile> = input
                .files
                .iter()
                .filter(|file| file.source_id == layer.source_id)
                .collect();
            PlanArea {
                area_id: format!("layer-{}", slug(&layer.label, &layer.source_id)),
                title: format!("Layer {}", layer.label),
                kind: "layer",
                layer: Some(layer.source_id.clone()),
                paths: Vec::new(),
                files: files.len(),
                changed_lines: files.iter().map(|file| file.changed_lines).sum(),
            }
        })
        .collect()
}

fn slug(label: &str, fallback: &str) -> String {
    let slug: String = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect();
    if slug.is_empty() {
        fallback.to_owned()
    } else {
        slug
    }
}

/// The package root of `path`: the longest root that contains it.
fn package_of<'a>(path: &str, roots: &'a [String]) -> Option<&'a str> {
    roots
        .iter()
        .filter(|root| root.is_empty() || path.starts_with(&format!("{root}/")))
        .max_by_key(|root| root.len())
        .map(String::as_str)
}

/// One cluster of files.
#[derive(Debug, Clone)]
struct Cluster {
    key: String,
    files: usize,
    changed_lines: u64,
}

/// Files grouped by package root (a root package counts as none), else by top directory,
/// `level` directories deeper; files directly in a group's directory stay in it.
fn clusters(files: &[PlanFile], roots: &[String], level: usize) -> Vec<Cluster> {
    let mut grouped: BTreeMap<String, Cluster> = BTreeMap::new();
    for file in files {
        let base = package_of(&file.path, roots).filter(|root| !root.is_empty());
        let (prefix, rest) = base.map_or(("", file.path.as_str()), |root| {
            (root, &file.path[root.len() + 1..])
        });
        let mut parts: Vec<&str> = rest.split('/').collect();
        parts.pop();
        let take = if base.is_some() { level } else { level + 1 };
        let extra = parts
            .iter()
            .take(take)
            .copied()
            .collect::<Vec<_>>()
            .join("/");
        let key = match (prefix.is_empty(), extra.is_empty()) {
            (true, true) => "(root)".to_owned(),
            (true, false) => extra,
            (false, true) => prefix.to_owned(),
            (false, false) => format!("{prefix}/{extra}"),
        };
        let cluster = grouped.entry(key.clone()).or_insert(Cluster {
            key,
            files: 0,
            changed_lines: 0,
        });
        cluster.files += 1;
        cluster.changed_lines += file.changed_lines;
    }
    grouped.into_values().collect()
}

/// Clusters packed into at most `max` areas, largest first into the lightest area.
fn balance(mut clusters: Vec<Cluster>, max: usize) -> Vec<PlanArea> {
    clusters.sort_by(|a, b| {
        b.changed_lines
            .cmp(&a.changed_lines)
            .then_with(|| a.key.cmp(&b.key))
    });
    let bins = clusters.len().min(max.max(1));
    let mut areas: Vec<Vec<Cluster>> = vec![Vec::new(); bins];
    for cluster in clusters {
        let lightest = areas
            .iter()
            .enumerate()
            .min_by_key(|(index, area)| {
                (
                    area.iter()
                        .map(|cluster| cluster.changed_lines)
                        .sum::<u64>(),
                    *index,
                )
            })
            .map_or(0, |(index, _)| index);
        areas[lightest].push(cluster);
    }
    areas
        .into_iter()
        .filter(|area| !area.is_empty())
        .map(|area| {
            let keys: Vec<String> = area.iter().map(|cluster| cluster.key.clone()).collect();
            let area_id = if keys.len() == 1 {
                keys[0].clone()
            } else {
                format!("{}+{}", keys[0], keys.len() - 1)
            };
            PlanArea {
                title: keys.join(", "),
                area_id,
                kind: "paths",
                layer: None,
                files: area.iter().map(|cluster| cluster.files).sum(),
                changed_lines: area.iter().map(|cluster| cluster.changed_lines).sum(),
                paths: keys,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, source: &str, lines: u64) -> PlanFile {
        PlanFile {
            path: path.to_owned(),
            source_id: source.to_owned(),
            changed_lines: lines,
        }
    }

    #[test]
    fn a_small_review_gets_one_agent() {
        let input = PlanInput {
            files: vec![file("src/a.rs", "s", 40), file("src/b.rs", "s", 10)],
            ..PlanInput::default()
        };
        let plan = plan(&input, &GuideConfig::default(), None, None);
        assert_eq!(plan.strategy, GuideStrategy::Single);
        assert_eq!(plan.areas, Vec::new());
        assert!(!plan.estimate.needs_confirmation);
        assert!(!plan.verifier, "standard depth has no verifier by default");
    }

    #[test]
    fn a_stack_of_four_layers_gets_one_child_per_layer() {
        let layers: Vec<PlanLayer> = (1..=4)
            .map(|n| PlanLayer {
                source_id: format!("layer-{n}"),
                label: format!("#10{n}"),
            })
            .collect();
        let files = (1..=4)
            .map(|n| file(&format!("src/f{n}.rs"), &format!("layer-{n}"), 20))
            .collect();
        let input = PlanInput {
            files,
            layers,
            ..PlanInput::default()
        };
        let plan = plan(
            &input,
            &GuideConfig::default(),
            None,
            Some(GuideDepth::Deep),
        );
        assert_eq!(plan.strategy, GuideStrategy::PerLayer);
        assert_eq!(plan.areas.len(), 4);
        assert_eq!(plan.areas[0].layer.as_deref(), Some("layer-1"));
        assert_eq!(plan.areas[0].area_id, "layer-101");
        assert!(plan.verifier, "deep guides are verified by default");
    }

    #[test]
    fn a_large_multi_package_review_is_clustered_by_package_and_balanced() {
        let mut files = Vec::new();
        for (package, count, lines) in [
            ("crates/net", 30, 100),
            ("crates/store", 20, 60),
            ("crates/ui", 10, 30),
        ] {
            for n in 0..count {
                files.push(file(&format!("{package}/src/m{n}.rs"), "s", lines));
            }
        }
        for n in 0..5 {
            files.push(file(&format!("docs/page{n}.md"), "s", 10));
        }
        let input = PlanInput {
            files,
            package_roots: vec![
                String::new(),
                "crates/net".to_owned(),
                "crates/store".to_owned(),
                "crates/ui".to_owned(),
            ],
            ..PlanInput::default()
        };
        let config = GuideConfig::default();
        let plan = plan(&input, &config, None, None);
        assert_eq!(plan.strategy, GuideStrategy::Areas);
        let ids: Vec<&str> = plan
            .areas
            .iter()
            .map(|area| area.area_id.as_str())
            .collect();
        assert_eq!(ids, ["crates/net", "crates/store", "crates/ui", "docs"]);
        assert_eq!(plan.metrics.packages, 4);
        let capped = super::plan(
            &input,
            &GuideConfig {
                max_children: 2,
                ..config
            },
            None,
            None,
        );
        assert_eq!(capped.areas.len(), 2);
        let lines: Vec<u64> = capped.areas.iter().map(|area| area.changed_lines).collect();
        assert_eq!(lines.iter().sum::<u64>(), 3000 + 1200 + 300 + 50);
        assert!(lines[0].abs_diff(lines[1]) <= 3000, "{lines:?}");
    }

    #[test]
    fn one_package_is_split_by_its_directories() {
        let files: Vec<PlanFile> = (0..60)
            .map(|n| file(&format!("crates/net/src/part{}/m{n}.rs", n % 3), "s", 50))
            .collect();
        let input = PlanInput {
            files,
            package_roots: vec!["crates/net".to_owned()],
            ..PlanInput::default()
        };
        let plan = plan(&input, &GuideConfig::default(), None, None);
        assert_eq!(plan.strategy, GuideStrategy::Areas);
        assert_eq!(plan.areas.len(), 3, "{:?}", plan.areas);
        assert!(plan.estimate.needs_confirmation || plan.estimate.estimated_tokens > 0);
    }

    #[test]
    fn the_strategy_can_be_forced() {
        let input = PlanInput {
            files: vec![file("src/a.rs", "s", 10)],
            ..PlanInput::default()
        };
        let forced = plan(
            &input,
            &GuideConfig::default(),
            Some(StrategyChoice::FanOut),
            None,
        );
        assert_eq!(forced.strategy, GuideStrategy::Areas);
        assert_eq!(forced.areas.len(), 1);
    }
}
