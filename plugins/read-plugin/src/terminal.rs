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
                let width = width.max(1);
                let height = height.max(1);
                terminal.resize(Rect::new(0, 0, width, height));
                pager.resize(width, height);
                pager.draw(&mut terminal)?;
            }
            Event::FocusGained
            | Event::FocusLost
            | Event::Mouse(_)
            | Event::Paste(_)
            | Event::Key(_) => {}
        }
    }

    let _stdout = guard.leave()?;
    Ok(())
}

fn ensure_controlling_terminal() -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map(|_| ())
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
        WindowsConsoleInputGuard::probe()
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
    Unix(std::fs::File),
}

impl EventInput {
    fn controlling_terminal() -> io::Result<Self> {
        #[cfg(unix)]
        {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .map(Self::Unix)
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
            Self::Unix(terminal) => read_unix_event(terminal),
        }
    }
}

#[cfg(unix)]
fn read_unix_event(terminal: &std::fs::File) -> io::Result<Event> {
    use std::io::Read as _;
    let mut terminal = terminal;
    let mut byte = [0_u8; 1];
    terminal.read_exact(&mut byte)?;
    parse_unix_key(byte[0], &mut terminal)
}

#[cfg(unix)]
fn parse_unix_key(first: u8, terminal: &mut &std::fs::File) -> io::Result<Event> {
    let event = match first {
        0x1b => parse_escape_sequence(terminal)?,
        b'\r' | b'\n' => KeyEvent::new(CrosstermKeyCode::Enter, KeyModifiers::NONE),
        b' ' => KeyEvent::new(CrosstermKeyCode::Char(' '), KeyModifiers::NONE),
        0x01..=0x1a => KeyEvent::new(
            CrosstermKeyCode::Char(char::from(b'a' + first - 1)),
            KeyModifiers::CONTROL,
        ),
        byte if byte.is_ascii() => {
            KeyEvent::new(CrosstermKeyCode::Char(char::from(byte)), KeyModifiers::NONE)
        }
        _ => KeyEvent::new(CrosstermKeyCode::Null, KeyModifiers::NONE),
    };
    Ok(Event::Key(event))
}

#[cfg(unix)]
fn parse_escape_sequence(terminal: &mut &std::fs::File) -> io::Result<KeyEvent> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let mut poll_fd = libc::pollfd {
        fd: terminal.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&raw mut poll_fd, 1, 20) } <= 0 || poll_fd.revents & libc::POLLIN == 0 {
        return Ok(KeyEvent::new(CrosstermKeyCode::Esc, KeyModifiers::NONE));
    }
    let mut second = [0_u8; 1];
    terminal.read_exact(&mut second)?;
    if second[0] != b'[' {
        return Ok(KeyEvent::new(CrosstermKeyCode::Esc, KeyModifiers::NONE));
    }
    let mut sequence = Vec::with_capacity(4);
    loop {
        let mut byte = [0_u8; 1];
        terminal.read_exact(&mut byte)?;
        sequence.push(byte[0]);
        if byte[0].is_ascii_alphabetic() || byte[0] == b'~' || sequence.len() == 4 {
            break;
        }
    }
    let code = match sequence.as_slice() {
        b"A" => CrosstermKeyCode::Up,
        b"B" => CrosstermKeyCode::Down,
        b"H" | b"1~" => CrosstermKeyCode::Home,
        b"F" | b"4~" => CrosstermKeyCode::End,
        b"5~" => CrosstermKeyCode::PageUp,
        b"6~" => CrosstermKeyCode::PageDown,
        _ => CrosstermKeyCode::Null,
    };
    Ok(KeyEvent::new(code, KeyModifiers::NONE))
}

struct TerminalGuard<W: Write = std::io::Stdout> {
    stdout: Option<W>,
    raw_mode: bool,
    alternate_screen: bool,
    #[cfg(windows)]
    terminal_input: Option<WindowsConsoleInputGuard>,
}

impl TerminalGuard<std::io::Stdout> {
    fn enter() -> io::Result<Self> {
        let mut guard = Self {
            stdout: Some(std::io::stdout()),
            raw_mode: false,
            alternate_screen: false,
            #[cfg(windows)]
            terminal_input: Some(WindowsConsoleInputGuard::enter()?),
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
        )?;
        #[cfg(windows)]
        if let Some(mut terminal_input) = self.terminal_input.take() {
            terminal_input.restore()?;
        }
        Ok(())
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

    #[test]
    fn terminal_cleanup_emits_restoration_in_order_and_is_idempotent() {
        let mut writer = Some(Vec::new());
        let mut alternate_screen = true;
        let mut raw_mode = false;

        leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).unwrap();
        let output = writer.as_ref().unwrap();
        let output = String::from_utf8_lossy(output);
        let reset = output.find("\u{1b}[0m").expect("style reset");
        let show_cursor = output.find("\u{1b}[?25h").expect("show cursor");
        let line_wrap = output.find("\u{1b}[?7h").expect("line wrap");
        let leave_screen = output.find("\u{1b}[?1049l").expect("leave screen");
        assert!(reset < show_cursor && show_cursor < line_wrap && line_wrap < leave_screen);
        assert!(!alternate_screen);

        let length = writer.as_ref().unwrap().len();
        leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode).unwrap();
        assert_eq!(writer.as_ref().unwrap().len(), length);
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("cleanup failed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_cleanup_clears_acquired_state_after_writer_failure() {
        let mut writer = Some(FailingWriter);
        let mut alternate_screen = true;
        let mut raw_mode = false;

        let error = leave_terminal_state(&mut writer, &mut alternate_screen, &mut raw_mode)
            .expect_err("writer failure should propagate");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!alternate_screen);
        assert!(!raw_mode);
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
                #[cfg(windows)]
                terminal_input: None,
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
}

#[cfg(windows)]
struct WindowsConsoleInputGuard {
    previous: windows_sys::Win32::Foundation::HANDLE,
    console: windows_sys::Win32::Foundation::HANDLE,
    active: bool,
}

#[cfg(windows)]
impl WindowsConsoleInputGuard {
    fn probe() -> io::Result<()> {
        let mut guard = Self::enter()?;
        guard.restore()
    }

    fn enter() -> io::Result<Self> {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetStdHandle};

        if std::io::stdin().is_terminal() {
            return Ok(Self {
                previous: std::ptr::null_mut(),
                console: std::ptr::null_mut(),
                active: false,
            });
        }

        let name = "CONIN$\0".encode_utf16().collect::<Vec<_>>();
        let console = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if console == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "`bcode read --interactive` requires a controlling console for keyboard input: {error}"
                ),
            ));
        }
        let previous = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if unsafe { SetStdHandle(STD_INPUT_HANDLE, console) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(console);
            }
            return Err(io::Error::new(
                error.kind(),
                format!("failed to select controlling console input: {error}"),
            ));
        }
        Ok(Self {
            previous,
            console,
            active: true,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        use windows_sys::Win32::System::Console::{STD_INPUT_HANDLE, SetStdHandle};

        if !self.active {
            return Ok(());
        }
        let restored = unsafe { SetStdHandle(STD_INPUT_HANDLE, self.previous) };
        let restore_error = (restored == 0).then(io::Error::last_os_error);
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.console);
        }
        self.active = false;
        restore_error.map_or(Ok(()), Err)
    }
}

#[cfg(windows)]
impl Drop for WindowsConsoleInputGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
