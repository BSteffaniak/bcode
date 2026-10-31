#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Export a real terminal capture as a styled BMUX grid for acceptance screenshots.
//! Usage: `render_preview CAPTURE COLUMNS ROWS` (JSON on stdout).

use bmux_terminal_grid::{GridLimits, GridSnapshot, TerminalGridStream};
use std::{
    io::{Read as _, Write as _},
    path::PathBuf,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = PathBuf::from(arguments.next().ok_or("capture path required")?);
    let width: u16 = arguments
        .next()
        .ok_or("columns required")?
        .to_str()
        .ok_or("invalid columns")?
        .parse()?;
    let height: u16 = arguments
        .next()
        .ok_or("rows required")?
        .to_str()
        .ok_or("invalid rows")?
        .parse()?;
    if arguments.next().is_some() || width == 0 || height == 0 || width > 300 || height > 150 {
        return Err("expected capture path and nonzero bounded columns/rows".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("capture exceeds 8 MiB".into());
    }
    let mut stream = TerminalGridStream::new(width, height, GridLimits::default())?;
    stream.process(&bytes);
    let snapshot =
        GridSnapshot::try_from_grid(stream.grid(), 0, usize::from(height), 16 * 1024 * 1024)
            .ok_or("styled grid exceeds snapshot budget")?;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &snapshot)?;
    stdout.write_all(b"\n")?;
    Ok(())
}
