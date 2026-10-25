#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Standalone Vox executable; all commands are implemented in the library frontend.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    match bcode_vox::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message = bcode_vox::cli::diagnostic(&error);
            let mut stderr = std::io::stderr().lock();
            // Never render backend errors, paths or user-supplied argument text.
            // Errors belong to stderr, leaving transcript stdout unpolluted.
            if std::env::args_os().any(|argument| argument == "--json") {
                let _ = serde_json::to_writer(&mut stderr, &serde_json::json!({"error": message}));
                let _ = writeln!(stderr);
            } else {
                let _ = writeln!(stderr, "vox: {message}");
            }
            match error {
                bcode_vox::Error::Cancelled => ExitCode::from(130),
                bcode_vox::Error::Invalid(_) => ExitCode::from(2),
                _ => ExitCode::FAILURE,
            }
        }
    }
}
