//! Statically bundled metrics CLI contribution.

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
    /// Metrics event log path. Defaults to Bcode's persisted metrics store.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Repository path used for plugin surface context.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        command: MetricsCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = MetricsCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        let mut options = BTreeMap::new();
        if let Some(path) = cli.path {
            options.insert(
                "metrics_path".to_owned(),
                path.to_string_lossy().into_owned(),
            );
        }
        Ok(StaticCliOutcome {
            host_action: Some(StaticCliHostAction::OpenTuiSurface {
                surface_kind: METRICS_DASHBOARD_SURFACE_KIND.to_owned(),
                repo_path: Some(cli.repo),
                options,
            }),
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
