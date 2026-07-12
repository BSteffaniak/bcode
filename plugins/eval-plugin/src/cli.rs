//! Statically bundled eval CLI contribution.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_plugin_sdk::{
    StaticCliFuture, StaticCliHostAction, StaticCliOutcome, StaticCliRegistration,
};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::tui;

#[derive(Debug, Parser)]
#[command(name = "eval", about = "Run and inspect evaluation suites")]
struct EvalCli {
    #[command(subcommand)]
    command: EvalCommand,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Eval(#[from] bcode_eval::EvalError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] bcode_client::ClientError),
    #[error("eval check failed: {0}")]
    EvalCheckFailed(String),
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: true,
        command: EvalCli::command,
        invoke,
    }
}
fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = EvalCli::from_arg_matches(&matches).map_err(|e| e.to_string())?;
        run(cli.command).await.map_err(|e| e.to_string())
    })
}
fn surface_outcome(kind: &str, run: Option<PathBuf>) -> StaticCliOutcome {
    let mut options = BTreeMap::new();
    if let Some(run) = run {
        options.insert("run".into(), run.to_string_lossy().into_owned());
    }
    StaticCliOutcome {
        host_action: Some(StaticCliHostAction::OpenTuiSurface {
            surface_kind: kind.into(),
            repo_path: Some(PathBuf::from(".")),
            options,
        }),
    }
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Validate an eval suite manifest.
    Validate {
        /// Suite TOML path.
        suite: PathBuf,
    },
    /// Run an eval suite.
    Run {
        /// Suite TOML path.
        suite: PathBuf,
        /// Output root for run directories.
        #[arg(long, default_value = "target/bcode-evals/runs")]
        output_root: PathBuf,
        /// Optional explicit run id.
        #[arg(long)]
        run_id: Option<String>,
        /// Print the full JSON summary.
        #[arg(long)]
        json: bool,
        /// Exit non-zero if overall pass rate is below this threshold.
        #[arg(long)]
        fail_under_pass_rate: Option<f64>,
    },
    /// Print a run summary.
    Report {
        /// Run directory or summary.json path.
        run: PathBuf,
        /// Print JSON instead of Markdown-ish text.
        #[arg(long)]
        json: bool,
    },
    /// Compare runs.
    Compare {
        /// Run directories or summary.json paths.
        runs: Vec<PathBuf>,
        /// Optional output JSON path.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Optional Markdown output path.
        #[arg(long)]
        markdown: Option<PathBuf>,
        /// Exit non-zero if any variant is below this pass-rate threshold.
        #[arg(long)]
        fail_under_pass_rate: Option<f64>,
    },
    /// Set a baseline for a suite from a completed run.
    Baseline {
        /// Eval output root.
        #[arg(long, default_value = "target/bcode-evals")]
        output_root: PathBuf,
        /// Run directory or summary.json path.
        run: PathBuf,
    },
    /// Check a candidate run for regressions against a baseline.
    Regressions {
        /// Baseline JSON path or baseline run summary.
        baseline: PathBuf,
        /// Candidate run directory or summary.json path.
        candidate: PathBuf,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
        /// Exit non-zero when regressions are detected.
        #[arg(long)]
        fail_on_regression: bool,
    },
    /// Create a new eval suite skeleton.
    InitSuite {
        /// Destination directory.
        directory: PathBuf,
        /// Suite id.
        #[arg(long)]
        id: String,
        /// Suite name.
        #[arg(long)]
        name: String,
    },
    /// Bless an expected patch from a completed repetition diff.
    Bless {
        /// Source diff.patch path from a run artifact.
        diff: PathBuf,
        /// Destination expected patch path.
        expected_patch: PathBuf,
    },
    /// List eval suites under a root directory.
    List {
        /// Root directory to scan.
        #[arg(default_value = "fixtures/evals")]
        root: PathBuf,
    },
    /// Add a fixture-backed case skeleton to a suite directory.
    AddCase {
        /// Suite directory containing suite.toml.
        suite_dir: PathBuf,
        /// Case id.
        #[arg(long)]
        id: String,
        /// Case name.
        #[arg(long)]
        name: String,
        /// Optional fixture directory to copy.
        #[arg(long)]
        from_dir: Option<PathBuf>,
    },
    /// Capture expected.patch from a run artifact diff path.
    CaptureExpected {
        /// Source diff.patch path.
        diff: PathBuf,
        /// Suite directory.
        suite_dir: PathBuf,
        /// Case id to update.
        case_id: String,
    },
    /// Export an existing session history to replay JSONL.
    ReplaySession {
        /// Session id to export.
        session_id: String,
        /// Output JSONL path.
        output: PathBuf,
    },
    /// List model-callable tools exposed by loaded plugins.
    Tools {
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Open the eval run viewer TUI.
    Viewer {
        /// Optional run directory or summary.json path.
        /// When omitted, the run picker is opened.
        run: Option<PathBuf>,
    },
    /// Open the eval run picker TUI directly.
    ViewerPicker,
    /// Manage multi-generation eval improvement campaigns.
    Improve {
        /// Improvement campaign command.
        #[command(subcommand)]
        command: EvalImproveCommand,
    },
}

#[derive(Debug, Subcommand)]
enum EvalImproveCommand {
    /// Start an improvement campaign for a suite.
    Start {
        /// Suite TOML path.
        suite: PathBuf,
        /// Output root for campaign directories.
        #[arg(long, default_value = "target/bcode-evals/improvements")]
        output_root: PathBuf,
        /// Optional explicit campaign id.
        #[arg(long)]
        campaign_id: Option<String>,
        /// Optional campaign name.
        #[arg(long)]
        name: Option<String>,
        /// Optional baseline run directory or summary.json path.
        #[arg(long)]
        baseline_run: Option<PathBuf>,
        /// Objective: progression, parent-comparison, baseline-comparison, or variant-comparison.
        #[arg(long, default_value = "progression")]
        objective: String,
    },
    /// Record a manual generation with an optional run and patch.
    Record {
        /// Campaign directory or campaign.json path.
        campaign: PathBuf,
        /// Parent generation id. Defaults to campaign latest.
        #[arg(long)]
        parent: Option<String>,
        /// Branch name.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Delta kind, for example `system_prompt_overlay` or `tool_behavior_patch`.
        #[arg(long, default_value = "mixed")]
        kind: String,
        /// Delta summary.
        #[arg(long)]
        summary: String,
        /// Optional run directory or summary.json path.
        #[arg(long)]
        run: Option<PathBuf>,
        /// Optional patch path to copy into generation artifacts.
        #[arg(long)]
        patch: Option<PathBuf>,
        /// Risk level: low, medium, or high.
        #[arg(long, default_value = "medium")]
        risk: String,
        /// Optional rationale.
        #[arg(long)]
        rationale: Option<String>,
    },
    /// Print campaign status and generation timeline.
    Status {
        /// Campaign directory or campaign.json path.
        campaign: PathBuf,
        /// Print JSON output.
        #[arg(long)]
        json: bool,
    },
}

#[allow(clippy::too_many_lines)]
async fn run(command: EvalCommand) -> Result<StaticCliOutcome, CliError> {
    match command {
        EvalCommand::Validate { suite } => {
            let suite = bcode_eval::load_suite(suite)?;
            println!("valid eval suite: {} ({})", suite.id, suite.name);
        }
        EvalCommand::Run {
            suite,
            output_root,
            run_id,
            json,
            fail_under_pass_rate,
        } => {
            let options = bcode_eval::EvalRunOptions {
                suite_path: suite,
                output_root,
                run_id,
            };
            let result = bcode_eval::run_suite(&options)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("eval run: {}", result.manifest.run_id);
                println!(
                    "summary: {}",
                    display_from_current_dir(result.manifest.output_dir.join("summary.md"))
                );
                print!("{}", bcode_eval::render_terminal_summary(&result));
                println!("passed: {}", result.passed);
            }
            if let Some(threshold) = fail_under_pass_rate {
                let pass_rate = result
                    .variants
                    .iter()
                    .map(|variant| variant.pass_rate)
                    .fold(1.0_f64, f64::min);
                if pass_rate < threshold {
                    return Err(CliError::EvalCheckFailed(format!(
                        "minimum variant pass rate {:.2}% is below threshold {:.2}%",
                        pass_rate * 100.0,
                        threshold * 100.0
                    )));
                }
            }
        }
        EvalCommand::Report { run, json } => {
            let result = bcode_eval::load_run_result(run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let markdown = bcode_eval::render_summary_markdown(&result);
                std::fs::write(result.manifest.output_dir.join("summary.md"), &markdown)?;
                print!("{markdown}");
            }
        }
        EvalCommand::Compare {
            runs,
            output,
            markdown,
            fail_under_pass_rate,
        } => {
            let results = runs
                .iter()
                .map(bcode_eval::load_run_result)
                .collect::<Result<Vec<_>, _>>()?;
            let report = bcode_eval::compare_runs(&results);
            if let Some(output) = output {
                bcode_eval::write_comparison_report(&report, output)?;
            }
            if let Some(markdown) = markdown {
                std::fs::write(
                    markdown,
                    bcode_eval::render_runs_comparison_markdown(&results),
                )?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            if let Some(threshold) = fail_under_pass_rate
                && let Some(variant) = report
                    .variants
                    .iter()
                    .find(|variant| variant.pass_rate < threshold)
            {
                return Err(CliError::EvalCheckFailed(format!(
                    "variant {} pass rate {:.2}% is below threshold {:.2}%",
                    variant.variant_id,
                    variant.pass_rate * 100.0,
                    threshold * 100.0
                )));
            }
        }
        EvalCommand::Baseline { output_root, run } => {
            let baseline = bcode_eval::set_baseline(output_root, run)?;
            println!("baseline {} -> {}", baseline.suite_id, baseline.run_id);
        }
        EvalCommand::Regressions {
            baseline,
            candidate,
            json,
            fail_on_regression,
        } => {
            let report = bcode_eval::regression_report(baseline, candidate)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("regressed: {}", report.regressed);
                for diagnostic in &report.diagnostics {
                    println!("{:?}: {}", diagnostic.severity, diagnostic.message);
                }
            }
            if fail_on_regression && report.regressed {
                return Err(CliError::EvalCheckFailed(
                    "candidate run regressed against baseline".to_string(),
                ));
            }
        }
        EvalCommand::InitSuite {
            directory,
            id,
            name,
        } => {
            bcode_eval::init_suite(directory, &id, &name)?;
            println!("created eval suite {id}");
        }
        EvalCommand::Bless {
            diff,
            expected_patch,
        } => {
            bcode_eval::bless_expected_patch(diff, expected_patch)?;
            println!("blessed expected patch");
        }
        EvalCommand::List { root } => {
            for suite in bcode_eval::list_suites(root)? {
                let loaded = bcode_eval::load_suite(&suite)?;
                println!(
                    "{}\t{}\t{}",
                    loaded.id,
                    loaded.name,
                    display_from_current_dir(&suite)
                );
            }
        }
        EvalCommand::AddCase {
            suite_dir,
            id,
            name,
            from_dir,
        } => {
            bcode_eval::add_case(suite_dir, &id, &name, from_dir.as_deref())?;
            println!("added eval case {id}");
        }
        EvalCommand::CaptureExpected {
            diff,
            suite_dir,
            case_id,
        } => {
            bcode_eval::capture_expected_patch(diff, suite_dir, &case_id)?;
            println!("captured expected patch for {case_id}");
        }
        EvalCommand::ReplaySession { session_id, output } => {
            bcode_eval::export_session_replay(&session_id, &output)?;
            println!(
                "exported replay transcript to {}",
                display_from_current_dir(&output)
            );
        }
        EvalCommand::Tools { json } => {
            let tools = bcode_eval::list_loaded_tools()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tools)?);
            } else {
                for tool in tools {
                    println!(
                        "{}\t{:?}\t{}",
                        tool.name, tool.side_effect, tool.description
                    );
                }
            }
        }
        EvalCommand::Viewer { run } => {
            return Ok(surface_outcome(tui::EVAL_RUN_VIEWER_SURFACE_KIND, run));
        }
        EvalCommand::ViewerPicker => {
            return Ok(surface_outcome(tui::EVAL_RUN_PICKER_SURFACE_KIND, None));
        }
        EvalCommand::Improve { command } => {
            handle_eval_improve_command(command)?;
        }
    }
    Ok(StaticCliOutcome::default())
}

fn handle_eval_improve_command(command: EvalImproveCommand) -> Result<(), CliError> {
    match command {
        EvalImproveCommand::Start {
            suite,
            output_root,
            campaign_id,
            name,
            baseline_run,
            objective,
        } => {
            let campaign = bcode_eval::start_improvement_campaign(
                suite,
                bcode_eval::EvalImprovementStartOptions {
                    output_root,
                    campaign_id,
                    name,
                    baseline_run,
                    objective: parse_improvement_objective(&objective)?,
                },
            )?;
            println!("improvement campaign: {}", campaign.id);
            println!("path: {}", display_from_current_dir(&campaign.output_dir));
        }
        EvalImproveCommand::Record {
            campaign,
            parent,
            branch,
            kind,
            summary,
            run,
            patch,
            risk,
            rationale,
        } => {
            let generation = bcode_eval::record_improvement_generation(
                bcode_eval::EvalImprovementRecordOptions {
                    campaign,
                    parent_id: parent,
                    branch,
                    delta_kind: parse_improvement_delta_kind(&kind)?,
                    summary,
                    run,
                    patch,
                    overlays: Vec::new(),
                    affected_files: Vec::new(),
                    affected_surfaces: Vec::new(),
                    expected_impact: None,
                    risk: parse_improvement_risk(&risk)?,
                    rationale,
                },
            )?;
            println!("recorded generation {}", generation.id);
        }
        EvalImproveCommand::Status { campaign, json } => {
            let campaign = bcode_eval::load_improvement_campaign(campaign)?;
            let generations = bcode_eval::load_improvement_generations(&campaign.output_dir)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&(campaign, generations))?
                );
            } else {
                print!(
                    "{}",
                    bcode_eval::render_improvement_campaign_markdown(&campaign, &generations)
                );
            }
        }
    }
    Ok(())
}

fn parse_improvement_delta_kind(
    value: &str,
) -> Result<bcode_eval_models::EvalImprovementDeltaKind, CliError> {
    use bcode_eval_models::EvalImprovementDeltaKind as Kind;
    match value {
        "baseline" => Ok(Kind::Baseline),
        "system_prompt_overlay" => Ok(Kind::SystemPromptOverlay),
        "system_prompt_patch" => Ok(Kind::SystemPromptPatch),
        "tool_description_overlay" => Ok(Kind::ToolDescriptionOverlay),
        "tool_schema_patch" => Ok(Kind::ToolSchemaPatch),
        "tool_behavior_patch" => Ok(Kind::ToolBehaviorPatch),
        "agent_profile_overlay" => Ok(Kind::AgentProfileOverlay),
        "permission_policy_overlay" => Ok(Kind::PermissionPolicyOverlay),
        "model_change" => Ok(Kind::ModelChange),
        "eval_case_change" => Ok(Kind::EvalCaseChange),
        "judge_change" => Ok(Kind::JudgeChange),
        "scoring_change" => Ok(Kind::ScoringChange),
        "mixed" => Ok(Kind::Mixed),
        _ => Err(CliError::EvalCheckFailed(format!(
            "unknown improvement delta kind: {value}"
        ))),
    }
}

fn parse_improvement_risk(value: &str) -> Result<bcode_eval_models::EvalImprovementRisk, CliError> {
    use bcode_eval_models::EvalImprovementRisk as Risk;
    match value {
        "low" => Ok(Risk::Low),
        "medium" => Ok(Risk::Medium),
        "high" => Ok(Risk::High),
        _ => Err(CliError::EvalCheckFailed(format!(
            "unknown improvement risk: {value}"
        ))),
    }
}

fn parse_improvement_objective(
    value: &str,
) -> Result<bcode_eval_models::EvalImprovementObjective, CliError> {
    use bcode_eval_models::EvalImprovementObjective as Objective;
    match value {
        "progression" => Ok(Objective::Progression),
        "parent-comparison" | "parent_comparison" => Ok(Objective::ParentComparison),
        "baseline-comparison" | "baseline_comparison" => Ok(Objective::BaselineComparison),
        "variant-comparison" | "variant_comparison" => Ok(Objective::VariantComparison),
        _ => Err(CliError::EvalCheckFailed(format!(
            "unknown improvement objective: {value}"
        ))),
    }
}
