#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `bcode-review`: the bcode code review app as a standalone binary.
//!
//! It takes the same subcommands as `bcode review` and opens sessions with `bcode` from the
//! path.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bcode_code_review_ui::cli::ReviewCommand;
use clap::Parser;

/// Interactive terminal code review.
#[derive(Debug, Parser)]
#[command(name = "bcode-review", version, about)]
struct Args {
    /// Run as if started in this directory.
    #[arg(short = 'C', long = "path", value_name = "DIR")]
    path: Option<PathBuf>,
    /// Print which configuration files are read, in order, and exit.
    #[arg(long)]
    print_config_paths: bool,
    /// Start with every review's agents in permission bypass: their tools run without asking.
    #[arg(long = "dangerously-bypass-all-permissions", visible_alias = "yolo")]
    dangerously_bypass_all_permissions: bool,
    /// What to open or do; the review home when absent.
    #[command(subcommand)]
    command: Option<ReviewCommand>,
}

/// How long background work may run once the app has exited (a save finishing), before the
/// process exits anyway. The terminal is restored before this.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bcode-review: {error}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run());
    // Blocking work still running (a provider call, a large diff) would otherwise hold the
    // process open after the review closed; it gets a short grace period instead.
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    code
}

async fn run() -> ExitCode {
    let args = Args::parse();
    if let Some(path) = &args.path
        && let Err(error) = std::env::set_current_dir(path)
    {
        eprintln!("bcode-review: {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    if args.print_config_paths {
        let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = bcode_code_review_ui::config::load(&start);
        for file in &config.files {
            println!("{}", file.display());
        }
        for diagnostic in &config.diagnostics {
            eprintln!("bcode-review: {diagnostic}");
        }
        return ExitCode::SUCCESS;
    }
    let launch =
        bcode_code_review_ui::ReviewLaunch::with_bypass(args.dangerously_bypass_all_permissions);
    match Box::pin(bcode_code_review_ui::cli::run_with(
        args.command,
        None,
        launch,
    ))
    .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bcode-review: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_starts_the_review_with_its_agents_in_permission_bypass() {
        let args = Args::try_parse_from(["bcode-review", "--yolo"]).expect("--yolo parses");
        assert!(args.dangerously_bypass_all_permissions);
        let args = Args::try_parse_from(["bcode-review", "--dangerously-bypass-all-permissions"])
            .expect("the long form parses");
        assert!(args.dangerously_bypass_all_permissions);
        assert!(
            !Args::try_parse_from(["bcode-review"])
                .unwrap()
                .dangerously_bypass_all_permissions
        );
    }
}
