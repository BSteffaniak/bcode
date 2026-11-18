//! `bcode review`: the code review app, contributed by the code review plugin.
//!
//! The command line and the app live in `bcode_code_review_ui`; `bcode review` runs the app in
//! this process exactly as the standalone `bcode-review` binary does.

use bcode_code_review_ui::cli::{ReviewCli, requires_daemon};
use bcode_plugin_sdk::{
    StaticCliExecution, StaticCliFuture, StaticCliOutcome, StaticCliRegistration,
};
use clap::{CommandFactory, FromArgMatches};

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |matches| {
            ReviewCli::from_arg_matches(matches)
                .is_ok_and(|cli| requires_daemon(cli.command.as_ref()))
        },
        command: ReviewCli::command,
        invoke,
        invoke_with_execution: Some(invoke_with_execution),
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    invoke_with_execution(matches, StaticCliExecution::default())
}

/// `bcode review` under the launch's execution mode: `--dangerously-bypass-all-permissions`
/// starts the app with review-wide permission bypass on; `--disable-all-tools` does not apply,
/// since the review's agents work through their tools.
fn invoke_with_execution(
    matches: clap::ArgMatches,
    execution: StaticCliExecution,
) -> StaticCliFuture {
    Box::pin(async move {
        if execution.disables_tools() {
            return Err(
                "`bcode review` does not support --disable-all-tools: its agents work through their tools"
                    .to_owned(),
            );
        }
        let cli = ReviewCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        // Sessions opened from the review run in this same bcode program.
        let bcode = std::env::current_exe().ok();
        let launch =
            bcode_code_review_ui::ReviewLaunch::with_bypass(execution.bypasses_permissions());
        Box::pin(bcode_code_review_ui::cli::run_with(
            cli.command,
            bcode,
            launch,
        ))
        .await
        .map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_takes_the_launchs_execution_mode() {
        assert!(registration().invoke_with_execution.is_some());
    }

    #[test]
    fn no_review_command_needs_the_daemon_before_it_runs() {
        let registration = registration();
        for (args, daemon) in [
            (vec!["review"], false),
            (vec!["review", "staged", "--repo", "/tmp/review"], false),
            (vec!["review", "pr", "42"], false),
            (vec!["review", "publish", "--submit"], false),
            (vec!["review", "publish-github", "--submit"], false),
        ] {
            let matches = (registration.command)()
                .try_get_matches_from(&args)
                .unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!((registration.requires_daemon)(&matches), daemon, "{args:?}");
        }
    }
}
