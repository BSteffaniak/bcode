//! Generated theme gallery (`docs/theme-gallery.md`).
//!
//! The page is rendered by the `bcode_tui` `theme_gallery` example through the
//! real TUI sample render path. Running it as a separate Cargo invocation keeps
//! the TUI out of xtask's own dependency graph, so release and architecture
//! tasks do not compile it.

use std::env;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::{
    GeneratedWriteMode, Options, Result, format_error, workspace_root, write_generated_file,
};

/// Workspace-relative path of the generated page.
pub const GALLERY_PATH: &str = "docs/theme-gallery.md";

/// Render the gallery and write it, or with `--check` fail when it is stale.
pub fn run(options: &Options) -> Result<()> {
    let started = Instant::now();
    let page = render()?;
    let path = workspace_root().join(GALLERY_PATH);
    write_generated_file(&path, &page, options)?;
    let outcome = match options.generated_write_mode {
        GeneratedWriteMode::Check => "is up to date",
        GeneratedWriteMode::DryRun => "dry run finished",
        GeneratedWriteMode::Write => "written",
    };
    eprintln!(
        "theme gallery: {GALLERY_PATH} {outcome} ({:.1}s)",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn render() -> Result<String> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(workspace_root())
        .args([
            "run",
            "--quiet",
            "--package",
            "bcode_tui",
            "--example",
            "theme_gallery",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format_error(format!("failed to run the theme gallery: {error}")))?;
    if !output.status.success() {
        return Err(format_error(format!(
            "theme gallery renderer failed with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format_error(format!("theme gallery output is not UTF-8: {error}")))
}
