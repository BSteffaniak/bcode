use std::io::{self, IsTerminal as _, Write};

use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Terminal;
use crossterm::event::{Event, KeyEventKind};
#[cfg(unix)]
use crossterm::event::{KeyCode as CrosstermKeyCode, KeyEvent, KeyModifiers};

use crate::pager::Pager;

pub(crate) fn prepare_input() -> io::Result<()> {
    ensure_controlling_terminal()
}

pub(crate) fn run(markdown: String, styled: bool) -> io::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(io::Error::other(
            "`bcode read --interactive` requires terminal stdout",
        ));
    }
    ensure_controlling_terminal()?;

    let input = if std::io::stdin().is_terminal() {
        EventInput::Crossterm
    } else {
        EventInput::controlling_terminal()?
    };
    let mut guard = TerminalGuard::enter()?;
    let (width, height) = crossterm::terminal::size()?;
    let mut terminal = Terminal::new(
        guard
            .writer_mut()
            .ok_or_else(|| io::Error::other("terminal writer is unavailable"))?,
        Rect::new(0, 0, width.max(1), height.max(1)),
    );
    let mut pager = Pager::new(markdown, width, height, styled);
    pager.draw(&mut terminal)?;

    loop {
        match input.read_event()? {
            Event::Key(event)
                if matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                let Some(key) = bmux_tui::crossterm::key_from_crossterm(event) else {
                    continue;
                };
                if pager.handle_key(key) {
                    break;
                }
                pager.draw(&mut terminal)?;
            }
            Event::Resize(width, height) => {
                terminal.resize(Rect::new(0, 0, width.max(1), height.max(1)));
                pager.resize(width, height);
                pager.draw(&mut terminal)?;
            }
            _ => {}
        }
    }

    drop(terminal);
    let mut stdout = guard.leave()?;
    stdout.flush()
}

fn ensure_controlling_terminal() -> io::Result<()> {
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map(drop)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "`bcode read --interactive` requires a controlling terminal for keyboard input: {error}"
                    ),
                )
            })
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .map(drop)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "`bcode read --interactive` requires a controlling console for keyboard input: {error}"
                    ),
                )
            })
    }
    #[cfg(not(any(unix, windows)))]
    {
        if std::io::stdin().is_terminal() {
            Ok(())
        } else {
            Err(io::Error::other(
                "`bcode read --interactive` requires a controlling terminal for keyboard input",
            ))
        }
    }
}

enum EventInput {
    Crossterm,
    #[cfg(unix)]
    Unix {
        terminal: std::fs::File,
        last_size: std::cell::Cell<(u16, u16)>,
    },
}

impl EventInput {
    fn controlling_terminal() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let terminal = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?;
            let size = crossterm::terminal::size().unwrap_or((1, 1));
            Ok(Self::Unix {
                terminal,
                last_size: std::cell::Cell::new(size),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self::Crossterm)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::other(
                "`bcode read --interactive` requires terminal stdin on this platform",
            ))
        }
    }

    fn read_event(&self) -> io::Result<Event> {
        match self {
            Self::Crossterm => crossterm::event::read(),
            #[cfg(unix)]
            Self::Unix {
                terminal,
                last_size,
            } => read_unix_event(terminal, last_size),
        }
    }
}

#[cfg(unix)]
fn read_unix_event(
    terminal: &std::fs::File,
    last_size: &std::cell::Cell<(u16, u16)>,
) -> io::Result<Event> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    loop {
        let mut poll_fd = libc::pollfd {
            fd: terminal.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_result = unsafe { libc::poll(&raw mut poll_fd, 1, 100) };
        if poll_result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let size = crossterm::terminal::size().unwrap_or_else(|_| last_size.get());
        if size != last_size.get() {
            last_size.set(size);
            return Ok(Event::Resize(size.0, size.1));
        }
        if poll_result == 0 || poll_fd.revents & libc::POLLIN == 0 {
            continue;
        }
        let mut terminal = terminal;
        let mut byte = [0_u8; 1];
        terminal.read_exact(&mut byte)?;
        return parse_unix_key(byte[0], &mut terminal);
    }
}

#[cfg(unix)]
fn parse_unix_key(first: u8, terminal: &mut impl std::io::Read) -> io::Result<Event> {
    let event = match first {
        b'\x1b' => parse_unix_escape(terminal)?,
        b'\r' | b'\n' => KeyEvent::new(CrosstermKeyCode::Enter, KeyModifiers::NONE),
        0x03 => KeyEvent::new(CrosstermKeyCode::Char('c'), KeyModifiers::CONTROL),
        0x04 => KeyEvent::new(CrosstermKeyCode::Char('d'), KeyModifiers::CONTROL),
        byte if byte.is_ascii() => {
            KeyEvent::new(CrosstermKeyCode::Char(char::from(byte)), KeyModifiers::NONE)
        }
        _ => KeyEvent::new(CrosstermKeyCode::Null, KeyModifiers::NONE),
    };
    Ok(Event::Key(event))
}

#[cfg(unix)]
fn parse_unix_escape(terminal: &mut impl std::io::Read) -> io::Result<KeyEvent> {
    use std::io::ErrorKind;

    let mut sequence = [0_u8; 3];
    let read = match terminal.read(&mut sequence[..1]) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => 0,
        Err(error) => return Err(error),
    };
    if read == 0 || sequence[0] != b'[' {
        return Ok(KeyEvent::new(CrosstermKeyCode::Esc, KeyModifiers::NONE));
    }
    terminal.read_exact(&mut sequence[1..2])?;
    let length = if matches!(sequence[1], b'5' | b'6') {
        terminal.read_exact(&mut sequence[2..3])?;
        3
    } else {
        2
    };
    let code = match &sequence[..length] {
        b"[A" => CrosstermKeyCode::Up,
        b"[B" => CrosstermKeyCode::Down,
        b"[H" => CrosstermKeyCode::Home,
        b"[F" => CrosstermKeyCode::End,
        b"[5~" => CrosstermKeyCode::PageUp,
        b"[6~" => CrosstermKeyCode::PageDown,
        _ => CrosstermKeyCode::Null,
    };
    Ok(KeyEvent::new(code, KeyModifiers::NONE))
}

struct TerminalGuard<W: Write = std::io::Stdout> {
    stdout: Option<W>,
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalGuard<std::io::Stdout> {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            stdout: Some(std::io::stdout()),
            raw_mode: false,
            alternate_screen: false,
        };
        crossterm::terminal::enable_raw_mode()?;
        guard.raw_mode = true;

        let stdout = guard
            .stdout
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal writer is unavailable"))?;
        crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
        guard.alternate_screen = true;
        crossterm::execute!(
            stdout,
            crossterm::terminal::DisableLineWrap,
            crossterm::cursor::Hide,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        Ok(guard)
    }

    const fn writer_mut(&mut self) -> Option<&mut std::io::Stdout> {
        self.stdout.as_mut()
    }

    fn leave(mut self) -> io::Result<std::io::Stdout> {
        self.leave_inner()?;
        self.stdout
            .take()
            .ok_or_else(|| io::Error::other("terminal writer was already taken"))
    }
}

impl<W: Write> TerminalGuard<W> {
    fn leave_inner(&mut self) -> io::Result<()> {
        leave_terminal_state(
            &mut self.stdout,
            &mut self.alternate_screen,
            &mut self.raw_mode,
        )
    }
}

fn leave_terminal_state<W: Write>(
    writer: &mut Option<W>,
    alternate_screen: &mut bool,
    raw_mode: &mut bool,
) -> io::Result<()> {
    let mut first_error = None;
    if *alternate_screen {
        if let Some(writer) = writer {
            if let Err(error) = crossterm::execute!(
                writer,
                crossterm::style::ResetColor,
                crossterm::style::SetAttribute(crossterm::style::Attribute::Reset),
                crossterm::cursor::Show,
                crossterm::terminal::EnableLineWrap,
                crossterm::terminal::LeaveAlternateScreen
            ) {
                first_error = Some(error);
            } else if let Err(error) = writer.flush() {
                first_error = Some(error);
            }
        }
        *alternate_screen = false;
    }
    if *raw_mode {
        if let Err(error) = crossterm::terminal::disable_raw_mode()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        *raw_mode = false;
    }
    first_error.map_or(Ok(()), Err)
}

impl<W: Write> Drop for TerminalGuard<W> {
    fn drop(&mut self) {
        let _ = self.leave_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn parse(bytes: &[u8]) -> Event {
        use std::io::{Read as _, Seek as _, Write as _};

        let mut temporary = tempfile::tempfile().unwrap();
        temporary.write_all(bytes).unwrap();
        temporary.rewind().unwrap();
        let mut terminal = &temporary;
        let mut first = [0_u8; 1];
        terminal.read_exact(&mut first).unwrap();
        parse_unix_key(first[0], &mut terminal).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn unix_controlling_terminal_parser_supports_pager_keys() {
        use crossterm::event::KeyCode;

        for (bytes, expected) in [
            (&b"q"[..], KeyCode::Char('q')),
            (&b" "[..], KeyCode::Char(' ')),
            (&b"\x1b"[..], KeyCode::Esc),
            (&b"\x1b[A"[..], KeyCode::Up),
            (&b"\x1b[B"[..], KeyCode::Down),
            (&b"\x1b[H"[..], KeyCode::Home),
            (&b"\x1b[F"[..], KeyCode::End),
            (&b"\x1b[5~"[..], KeyCode::PageUp),
            (&b"\x1b[6~"[..], KeyCode::PageDown),
        ] {
            let Event::Key(event) = parse(bytes) else {
                panic!("expected key event for {bytes:?}");
            };
            assert_eq!(event.code, expected);
        }
    }

    #[test]
    fn terminal_cleanup_emits_restoration_in_order_and_is_idempotent() {
        let mut writer = Some(Vec::new());
        let mut alternate_screen = true;
        let mut raw_mode = false;
        leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).unwrap();
        assert!(!alternate_screen);
        let output = writer.as_ref().unwrap();
        let reset = output
            .windows(4)
            .position(|bytes| bytes == b"\x1b[0m")
            .unwrap();
        let cursor = output
            .windows(6)
            .position(|bytes| bytes == b"\x1b[?25h")
            .unwrap();
        let wrap = output
            .windows(5)
            .position(|bytes| bytes == b"\x1b[?7h")
            .unwrap();
        let leave = output
            .windows(8)
            .position(|bytes| bytes == b"\x1b[?1049l")
            .unwrap();
        assert!(reset < cursor && cursor < wrap && wrap < leave);

        let length = output.len();
        leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).unwrap();
        assert_eq!(writer.as_ref().unwrap().len(), length);
    }

    struct FailOnceWriter {
        writes: usize,
    }

    impl Write for FailOnceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                Err(io::Error::other("simulated terminal failure"))
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_cleanup_clears_acquired_state_after_writer_failure() {
        let mut writer = Some(FailOnceWriter { writes: 0 });
        let mut alternate_screen = true;
        let mut raw_mode = false;
        assert!(leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).is_err());
        assert!(!alternate_screen);

        assert!(leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).is_ok());
        assert_eq!(writer.unwrap().writes, 1);
    }

    #[test]
    fn terminal_guard_drop_restores_after_an_external_failure() {
        use std::sync::{Arc, Mutex};

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedWriter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(Mutex::new(Vec::new()));
        {
            let _guard = TerminalGuard {
                stdout: Some(SharedWriter(Arc::clone(&output))),
                raw_mode: false,
                alternate_screen: true,
            };
            let simulated_input_error = io::Error::other("input failed");
            assert_eq!(simulated_input_error.kind(), io::ErrorKind::Other);
        }
        let output = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(output.windows(8).any(|bytes| bytes == b"\x1b[?1049l"));
        drop(output);
    }

    #[cfg(unix)]
    #[test]
    fn unix_controlling_terminal_parser_preserves_control_modifiers() {
        let Event::Key(event) = parse(&[0x03]) else {
            panic!("expected key event");
        };
        assert_eq!(event.code, CrosstermKeyCode::Char('c'));
        assert!(event.modifiers.contains(KeyModifiers::CONTROL));
    }
}
