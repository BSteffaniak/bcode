#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_terminal_grid::{GridLimits, TerminalGridStream, visible_text};
use std::io::Read as _;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: bcode_terminal_grid_probe <capture>")?;
    let width = std::env::args()
        .nth(2)
        .map_or(Ok(120), |value| value.parse::<u16>())?;
    let height = std::env::args()
        .nth(3)
        .map_or(Ok(30), |value| value.parse::<u16>())?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    let mut stream = TerminalGridStream::new(width, height, GridLimits::default())?;
    stream.process(&bytes);
    print!("{}", visible_text(stream.grid(), 0, usize::from(height)));
    Ok(())
}
