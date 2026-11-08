//! Print the generated theme gallery page to standard output.
//!
//! Run through `cargo run -p xtask -- theme-gallery [--check]`, which writes
//! or verifies `docs/theme-gallery.md`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    match bcode_tui::theme_gallery::gallery_markdown() {
        Ok(page) => {
            let mut stdout = std::io::stdout().lock();
            if let Err(error) = stdout
                .write_all(page.as_bytes())
                .and_then(|()| stdout.flush())
            {
                eprintln!("error: failed to write the theme gallery: {error}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: failed to render the theme gallery: {error}");
            ExitCode::FAILURE
        }
    }
}
