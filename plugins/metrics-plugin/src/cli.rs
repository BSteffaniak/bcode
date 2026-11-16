//! Statically bundled metrics CLI contribution.

use bcode_plugin_sdk::path::display_from_current_dir;
use bcode_plugin_sdk::{
    StaticCliFuture, StaticCliHostAction, StaticCliOutcome, StaticCliRegistration,
};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::tui::METRICS_DASHBOARD_SURFACE_KIND;

#[derive(Debug, Parser)]
#[command(
    name = "metrics",
    about = "Open the persisted performance metrics dashboard"
)]
struct MetricsCli {
    /// Print the current in-memory metrics snapshot as JSON.
    #[arg(long)]
    json: bool,
    /// Print the persisted metrics report as JSON.
    #[arg(long)]
    report: bool,
    /// Analyze the persisted metrics report.
    #[arg(long)]
    analyze: bool,
    /// Write a standalone HTML dashboard.
    #[arg(long)]
    html: Option<PathBuf>,
    /// Metrics event log path. Defaults to Bcode's persisted metrics store.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Repository path used for plugin surface context.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |_| true,
        command: MetricsCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let outcome = invoke_portable(matches).await?;
        use std::io::Write as _;
        std::io::stdout()
            .write_all(&outcome.stdout)
            .map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome {
            host_action: outcome.host_action.map(|action| match action {
                bcode_command::cli::CliHostAction::OpenTuiSurface {
                    surface_kind,
                    repo_path,
                    options,
                } => StaticCliHostAction::OpenTuiSurface {
                    surface_kind,
                    repo_path,
                    options,
                },
                bcode_command::cli::CliHostAction::AttachSession { session_id } => {
                    StaticCliHostAction::AttachSession { session_id }
                }
            }),
        })
    })
}

pub fn invoke_portable(
    matches: clap::ArgMatches,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<bcode_command::cli::CliOutcome, String>>
            + Send
            + 'static,
    >,
> {
    Box::pin(async move {
        let cli = MetricsCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        if cli.json || cli.report || cli.analyze || cli.html.is_some() {
            let status = bcode_client::BcodeClient::default_endpoint()
                .server_status()
                .await
                .map_err(|error| error.to_string())?;
            let output = if let Some(path) = cli.html {
                let payload = serde_json::to_string_pretty(&status.metrics_report)
                    .map_err(|error| error.to_string())?;
                std::fs::write(&path, payload).map_err(|error| error.to_string())?;
                format!("metrics report: {}", display_from_current_dir(&path))
            } else if cli.analyze {
                serde_json::to_string_pretty(&bcode_metrics::analyze_metrics_report(
                    &status.metrics_report,
                ))
                .map_err(|error| error.to_string())?
            } else if cli.report {
                serde_json::to_string_pretty(&status.metrics_report)
                    .map_err(|error| error.to_string())?
            } else {
                serde_json::to_string_pretty(&status.metrics).map_err(|error| error.to_string())?
            };
            return Ok(bcode_command::cli::CliOutcome {
                stdout: format!("{output}\n").into_bytes(),
                ..bcode_command::cli::CliOutcome::default()
            });
        }
        let mut options = BTreeMap::new();
        if let Some(path) = cli.path {
            options.insert(
                "metrics_path".to_owned(),
                path.to_string_lossy().into_owned(),
            );
        }
        Ok(bcode_command::cli::CliOutcome {
            host_action: Some(bcode_command::cli::CliHostAction::OpenTuiSurface {
                surface_kind: METRICS_DASHBOARD_SURFACE_KIND.to_owned(),
                repo_path: Some(cli.repo),
                options,
            }),
            ..bcode_command::cli::CliOutcome::default()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dashboard_context() {
        let matches = MetricsCli::command()
            .try_get_matches_from([
                "metrics",
                "--repo",
                "/tmp/example",
                "--path",
                "/tmp/metrics.jsonl",
            ])
            .expect("metrics command should parse");
        let cli = MetricsCli::from_arg_matches(&matches).expect("matches should decode");

        assert_eq!(cli.repo, PathBuf::from("/tmp/example"));
        assert_eq!(cli.path, Some(PathBuf::from("/tmp/metrics.jsonl")));
    }
}
