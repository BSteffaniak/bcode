//! Statically bundled read CLI contribution.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, IsTerminal as _};
use std::path::PathBuf;

use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "read", about = "Render Markdown for the terminal")]
struct ReadCli {
    /// Markdown file to read, or `-` for stdin.
    path: Option<PathBuf>,
    /// Open an alternate-screen pager.
    #[arg(short, long)]
    interactive: bool,
    /// When to use document colors and text styles.
    #[arg(long, value_enum)]
    color: Option<ColorMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Always,
    Auto,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadSource {
    File(PathBuf),
    Stdin,
}

pub(crate) fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: false,
        command: ReadCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let cli = ReadCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        run(cli).map_err(|error| error.to_string())?;
        Ok(StaticCliOutcome::default())
    })
}

fn run(cli: ReadCli) -> io::Result<()> {
    let stdin_is_terminal = std::io::stdin().is_terminal();
    let source = resolve_source(cli.path, stdin_is_terminal)?;
    if cli.interactive {
        crate::terminal::prepare_input()?;
    }
    let markdown = load_source(&source)?;
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let styled = effective_color(cli.color, std::env::var_os("NO_COLOR").is_some())
        .uses_styles(stdout_is_terminal, cli.interactive);

    if cli.interactive {
        return crate::terminal::run(markdown, styled);
    }

    let width = if stdout_is_terminal {
        crossterm::terminal::size().map_or(80, |(width, _)| width.max(1))
    } else {
        80
    };
    let lines = render_markdown_lines(&markdown, MarkdownRenderOptions::new(width));
    crate::output::write_lines(&mut std::io::stdout().lock(), &lines, styled)
}

fn resolve_source(path: Option<PathBuf>, stdin_is_terminal: bool) -> io::Result<ReadSource> {
    match path {
        Some(path) if path.as_os_str() == OsStr::new("-") => Ok(ReadSource::Stdin),
        Some(path) => Ok(ReadSource::File(path)),
        None if stdin_is_terminal => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`bcode read` requires a file path or piped stdin",
        )),
        None => Ok(ReadSource::Stdin),
    }
}

fn load_source(source: &ReadSource) -> io::Result<String> {
    match source {
        ReadSource::File(path) => {
            let mut file = File::open(path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to open `{}`: {error}", path.display()),
                )
            })?;
            load_reader(&mut file, &format!("file `{}`", path.display()))
        }
        ReadSource::Stdin => load_reader(&mut std::io::stdin().lock(), "stdin"),
    }
}

fn load_reader(reader: &mut impl io::Read, label: &str) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).map_err(|error| {
        io::Error::new(error.kind(), format!("failed to read {label}: {error}"))
    })?;
    String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} is not valid UTF-8: {error}"),
        )
    })
}

const fn effective_color(explicit: Option<ColorMode>, no_color: bool) -> ColorMode {
    match explicit {
        Some(mode) => mode,
        None if no_color => ColorMode::Never,
        None => ColorMode::Always,
    }
}

impl ColorMode {
    const fn uses_styles(self, stdout_is_terminal: bool, interactive: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Auto => stdout_is_terminal || interactive,
            Self::Never => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn parses_all_input_and_color_forms() {
        let cli = parse(["read", "README.md"]);
        assert_eq!(cli.path, Some(PathBuf::from("README.md")));
        assert!(!cli.interactive);
        assert_eq!(cli.color, None);

        let cli = parse(["read", "-i", "--color", "never", "-"]);
        assert_eq!(cli.path, Some(PathBuf::from("-")));
        assert!(cli.interactive);
        assert_eq!(cli.color, Some(ColorMode::Never));

        let cli = parse(["read", "--", "--notes.md"]);
        assert_eq!(cli.path, Some(PathBuf::from("--notes.md")));
    }

    #[test]
    fn invalid_color_is_rejected() {
        let error = ReadCli::command()
            .try_get_matches_from(["read", "--color", "sometimes"])
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test]
    fn source_resolution_obeys_path_dash_and_tty_rules() {
        assert_eq!(
            resolve_source(Some(PathBuf::from("README.md")), true).unwrap(),
            ReadSource::File(PathBuf::from("README.md"))
        );
        assert_eq!(
            resolve_source(Some(PathBuf::from("-")), true).unwrap(),
            ReadSource::Stdin
        );
        assert_eq!(resolve_source(None, false).unwrap(), ReadSource::Stdin);
        assert_eq!(
            resolve_source(None, true).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn input_loading_handles_empty_no_newline_and_invalid_utf8() {
        assert_eq!(load_reader(&mut &b""[..], "stdin").unwrap(), "");
        assert_eq!(
            load_reader(&mut &b"no newline"[..], "stdin").unwrap(),
            "no newline"
        );
        let error = load_reader(&mut &[0xff][..], "stdin").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("stdin is not valid UTF-8"));
    }

    struct ChunkedReader {
        chunks: std::collections::VecDeque<io::Result<Vec<u8>>>,
    }

    impl io::Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let chunk = chunk?;
            let length = chunk.len().min(buffer.len());
            buffer[..length].copy_from_slice(&chunk[..length]);
            if length < chunk.len() {
                self.chunks.push_front(Ok(chunk[length..].to_vec()));
            }
            Ok(length)
        }
    }

    #[test]
    fn input_loading_reads_chunks_to_eof_and_propagates_mid_read_errors() {
        let mut reader = ChunkedReader {
            chunks: std::collections::VecDeque::from([
                Ok(b"chunk ".to_vec()),
                Ok(b"one".to_vec()),
                Ok(b" and two".to_vec()),
            ]),
        };
        assert_eq!(
            load_reader(&mut reader, "stdin").unwrap(),
            "chunk one and two"
        );

        let mut reader = ChunkedReader {
            chunks: std::collections::VecDeque::from([
                Ok(b"partial".to_vec()),
                Err(io::Error::other("reader failed")),
            ]),
        };
        let error = load_reader(&mut reader, "stdin").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("failed to read stdin"));
    }

    #[test]
    fn file_loading_reports_missing_directory_and_invalid_utf8_inputs() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing.md");
        let error = load_source(&ReadSource::File(missing.clone())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&missing.display().to_string()));

        let error = load_source(&ReadSource::File(temporary.path().to_path_buf())).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::IsADirectory | io::ErrorKind::PermissionDenied
        ));

        let invalid = temporary.path().join("invalid.md");
        std::fs::write(&invalid, [0xff]).unwrap();
        let error = load_source(&ReadSource::File(invalid.clone())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(&invalid.display().to_string()));
    }

    #[test]
    fn color_precedence_and_tty_behavior_are_explicit() {
        assert_eq!(effective_color(None, false), ColorMode::Always);
        assert_eq!(effective_color(None, true), ColorMode::Never);
        assert_eq!(
            effective_color(Some(ColorMode::Always), true),
            ColorMode::Always
        );
        assert!(ColorMode::Always.uses_styles(false, false));
        assert!(!ColorMode::Auto.uses_styles(false, false));
        assert!(ColorMode::Auto.uses_styles(true, false));
        assert!(ColorMode::Auto.uses_styles(false, true));
        assert!(!ColorMode::Never.uses_styles(true, true));
    }

    fn parse<const N: usize>(args: [&str; N]) -> ReadCli {
        let matches = ReadCli::command().try_get_matches_from(args).unwrap();
        ReadCli::from_arg_matches(&matches).unwrap()
    }
}
