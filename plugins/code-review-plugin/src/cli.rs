//! `bcode review`: the code review app, contributed by the code review plugin.
//!
//! The command line and the app live in `bcode_code_review_ui`; `bcode review` runs the app in
//! this process exactly as the standalone `bcode-review` binary does.

use bcode_code_review_ui::cli::{ReviewCli, requires_daemon};
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches};

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |matches| {
            ReviewCli::from_arg_matches(matches)
                .is_ok_and(|cli| requires_daemon(cli.command.as_ref()))
        },
        command: ReviewCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = ReviewCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        // Sessions opened from the review run in this same bcode program.
        let bcode = std::env::current_exe().ok();
        bcode_code_review_ui::cli::run(cli.command, bcode)
            .await
            .map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
