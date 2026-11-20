#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `bcode-review`: the bcode code review app as a standalone binary.
//!
//! It takes the same subcommands as `bcode review`. Its sessions and plugin services are
//! served by Bcode's daemon hosted inside the app itself, unless `--bcode` or `BCODE_BIN`
//! names an installed Bcode to use instead. Sessions open in the installed Bcode's terminal
//! app either way.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bcode_code_review_ui::ReviewBcode;
use bcode_code_review_ui::cli::ReviewCommand;
use bcode_server::embedded_host::{EmbeddedDaemonHost, EmbeddedDaemonHostOptions};
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
    /// Use this installed Bcode executable and its daemon instead of the Bcode built into
    /// bcode-review. Takes precedence over `BCODE_BIN`.
    #[arg(long, value_name = "PATH")]
    bcode: Option<PathBuf>,
    /// What to open or do; the review home when absent.
    #[command(subcommand)]
    command: Option<ReviewCommand>,
}

/// How long background work may run once the app has exited (a save finishing), before the
/// process exits anyway. The terminal is restored before this.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long the embedded daemon may take to stop (releasing sessions and plugins) on exit.
const EMBEDDED_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Name of the embedded daemon's threads.
const DAEMON_THREAD_NAME: &str = "bcode-review-daemon";

/// Log of panics on the embedded daemon's threads, in Bcode's log directory.
const DAEMON_PANIC_LOG: &str = "bcode-review-embedded.log";

/// The Bcode the review uses: `--bcode`, else `BCODE_BIN`, each naming an installed Bcode;
/// otherwise the one built into bcode-review.
fn chosen_bcode(option: Option<PathBuf>, bcode_bin: Option<OsString>) -> ReviewBcode {
    option
        .or_else(|| {
            bcode_bin
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .map_or(ReviewBcode::Embedded { open_with: None }, |executable| {
            ReviewBcode::Installed {
                executable: Some(executable),
            }
        })
}

/// Host Bcode's daemon in this process: clients started by the review start it here.
fn embed_bcode() -> Result<(), String> {
    let options = EmbeddedDaemonHostOptions {
        server: bcode_server::InProcessServerOptions {
            static_plugins: bcode_bundled_plugins::static_bundled_plugins(),
            host_name: "bcode-review".to_owned(),
            bcode_cli: bcode_client::installed_bcode_executable(),
            // The review's lifetime bounds the daemon.
            idle_shutdown: false,
            private_env: Vec::new(),
        },
        thread_name: DAEMON_THREAD_NAME.to_owned(),
    };
    EmbeddedDaemonHost::register(options)
        .map(drop)
        .map_err(|error| error.to_string())?;
    log_daemon_panics();
    Ok(())
}

/// Keep panics on the embedded daemon's threads (which Bcode recovers from) off the review's
/// terminal: they go to [`DAEMON_PANIC_LOG`] instead.
fn log_daemon_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let on_daemon = std::thread::current()
            .name()
            .is_some_and(|name| name.starts_with(DAEMON_THREAD_NAME));
        if !on_daemon {
            previous(info);
            return;
        }
        let directory = bcode_config::default_state_dir().join("logs");
        if std::fs::create_dir_all(&directory).is_ok()
            && let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join(DAEMON_PANIC_LOG))
        {
            use std::io::Write as _;
            let _ = writeln!(log, "{info}");
        }
    }));
}

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
    let code = runtime.block_on(async {
        let code = run().await;
        if let Some(host) = EmbeddedDaemonHost::get() {
            host.shutdown(EMBEDDED_SHUTDOWN_GRACE).await;
        }
        code
    });
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
    let bcode = chosen_bcode(
        args.bcode,
        std::env::var_os(bcode_daemon_lifecycle::BCODE_BIN_ENV),
    );
    if matches!(bcode, ReviewBcode::Embedded { .. })
        && let Err(error) = embed_bcode()
    {
        eprintln!("bcode-review: {error}");
        return ExitCode::FAILURE;
    }
    match Box::pin(bcode_code_review_ui::cli::run_with(
        args.command,
        bcode,
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
    fn the_option_wins_over_bcode_bin_and_both_name_an_installed_bcode() {
        let installed = |path: &str| ReviewBcode::Installed {
            executable: Some(PathBuf::from(path)),
        };
        assert_eq!(
            chosen_bcode(Some("/opt/a/bcode".into()), Some("/opt/b/bcode".into())),
            installed("/opt/a/bcode")
        );
        assert_eq!(
            chosen_bcode(None, Some("/opt/b/bcode".into())),
            installed("/opt/b/bcode")
        );
    }

    #[test]
    fn without_a_choice_the_review_embeds_bcode() {
        assert_eq!(
            chosen_bcode(None, None),
            ReviewBcode::Embedded { open_with: None }
        );
        // An empty BCODE_BIN chooses nothing, as for Bcode's own lookup.
        assert_eq!(
            chosen_bcode(None, Some(OsString::new())),
            ReviewBcode::Embedded { open_with: None }
        );
    }

    #[test]
    fn bcode_names_an_executable() {
        let args = Args::try_parse_from(["bcode-review", "--bcode", "/opt/bcode"])
            .expect("--bcode parses");
        assert_eq!(args.bcode, Some(PathBuf::from("/opt/bcode")));
    }

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
