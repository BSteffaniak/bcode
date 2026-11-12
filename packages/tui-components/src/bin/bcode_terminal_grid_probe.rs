#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Replay a raw terminal capture through BMUX's terminal grid.
//!
//! Usage: `bcode_terminal_grid_probe CAPTURE [COLUMNS] [ROWS] [--styled]`. Prints the visible
//! text, or with `--styled` the visible screen as a styled `GridSnapshot` JSON document.

use bmux_terminal_grid::{GridLimits, GridSnapshot, TerminalGridStream, visible_text};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;

/// Largest capture and styled snapshot the probe accepts.
const BUDGET: usize = 16 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut styled = false;
    let mut positional = Vec::new();
    for argument in std::env::args().skip(1) {
        if argument == "--styled" {
            styled = true;
        } else {
            positional.push(argument);
        }
    }
    let path = positional
        .first()
        .map(PathBuf::from)
        .ok_or("usage: bcode_terminal_grid_probe <capture> [columns] [rows] [--styled]")?;
    let width = positional
        .get(1)
        .map_or(Ok(120), |value| value.parse::<u16>())?;
    let height = positional
        .get(2)
        .map_or(Ok(30), |value| value.parse::<u16>())?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(BUDGET as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > BUDGET {
        return Err("capture exceeds 16 MiB".into());
    }
    let mut stream = TerminalGridStream::new(width, height, GridLimits::default())?;
    stream.process(&bytes);
    let mut stdout = std::io::stdout().lock();
    if styled {
        let snapshot = GridSnapshot::try_from_grid(stream.grid(), 0, usize::from(height), BUDGET)
            .ok_or("styled grid exceeds snapshot budget")?;
        serde_json::to_writer(&mut stdout, &snapshot)?;
        stdout.write_all(b"\n")?;
    } else {
        write!(
            stdout,
            "{}",
            visible_text(stream.grid(), 0, usize::from(height))
        )?;
    }
    Ok(())
}
