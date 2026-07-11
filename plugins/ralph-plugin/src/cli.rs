//! Statically bundled Ralph CLI contribution.

use bcode_plugin_sdk::{
    StaticCliFuture, StaticCliHostAction, StaticCliOutcome, StaticCliRegistration,
};
use clap::{CommandFactory, FromArgMatches, Parser};
use std::path::PathBuf;

use crate::RALPH_HOME_SURFACE_KIND;

#[derive(Debug, Parser)]
#[command(name = "ralph", about = "Open the Ralph workflow UI")]
struct RalphCli {
    /// Repository path.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

pub(super) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: false,
        command: RalphCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = RalphCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome {
            host_action: Some(StaticCliHostAction::OpenTuiSurface {
                surface_kind: RALPH_HOME_SURFACE_KIND.to_owned(),
                repo_path: Some(cli.repo),
                options: std::collections::BTreeMap::new(),
            }),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repository_path() {
        let matches = RalphCli::command()
            .try_get_matches_from(["ralph", "--repo", "/tmp/example"])
            .expect("Ralph command should parse");
        let cli = RalphCli::from_arg_matches(&matches).expect("matches should decode");

        assert_eq!(cli.repo, PathBuf::from("/tmp/example"));
    }
}
