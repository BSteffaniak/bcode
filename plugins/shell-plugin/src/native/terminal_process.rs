//! The process a terminal-mode command runs as, behind one interface on every platform.
//!
//! [`spawn`] starts it on a pseudo-terminal (a Windows pseudo-console) through `bmux_pty` and
//! returns a [`TerminalChild`], which the invocation thread uses to wait for, resize, stop and
//! type into it, and the reader its terminal output is captured from on a thread of its own.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::ShellExecutionBudget;

/// How a terminal-mode process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExit {
    /// Exit code; 1 when a signal ended the process.
    pub code: u32,
    /// Name of the signal that ended the process, as the platform spells it.
    pub signal: Option<String>,
}

impl TerminalExit {
    /// Whether the process exited with code zero.
    pub const fn success(&self) -> bool {
        self.signal.is_none() && self.code == 0
    }
}

/// A running terminal-mode process, as the invocation thread controls it.
pub trait TerminalChild: Send {
    /// How the process ended, once it has (its output may still be arriving).
    ///
    /// # Errors
    /// * [`io::Error`] when the process cannot be waited for.
    fn try_wait(&mut self) -> io::Result<Option<TerminalExit>>;

    /// Kill the process and everything it started on its terminal.
    ///
    /// # Errors
    /// * [`io::Error`] when it cannot be signalled.
    fn terminate(&mut self) -> io::Result<()>;

    /// The terminal size as `(columns, rows)`.
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal cannot report it.
    fn size(&self) -> io::Result<(u16, u16)>;

    /// Resize the terminal; the process is notified.
    ///
    /// # Errors
    /// * [`io::Error`] when the terminal rejects the size.
    fn resize(&self, columns: u16, rows: u16) -> io::Result<()>;

    /// Write `bytes` to the terminal, as typing would.
    ///
    /// # Errors
    /// * [`io::ErrorKind::Unsupported`] where typing into a running command is not supported.
    /// * [`io::Error`] when the terminal can no longer be written.
    fn write_input(&self, bytes: &[u8]) -> io::Result<()>;
}

/// A started process: its control, and the reader its terminal output arrives on.
pub type Spawned = (Box<dyn TerminalChild>, Box<dyn Read + Send>);

/// What to run.
pub struct TerminalSpawn<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    /// Working directory; the home directory when `None`.
    pub cwd: Option<&'a Path>,
    /// Inherited environment variables the process must not see.
    pub remove_env: Vec<&'a str>,
    pub columns: u16,
    pub rows: u16,
}

/// The directory a command runs in: `cwd`, which must exist, or the home directory.
///
/// # Errors
/// * When `cwd` is not a directory: running elsewhere instead could act on the wrong files.
pub fn working_directory(cwd: Option<&Path>) -> Result<PathBuf, String> {
    match cwd {
        Some(cwd) if cwd.is_dir() => Ok(cwd.to_path_buf()),
        Some(cwd) => Err(format!(
            "working directory {} does not exist or is not a directory",
            cwd.display()
        )),
        None => home_directory().ok_or_else(|| "no working directory or home directory".to_owned()),
    }
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            {
                std::env::var_os("USERPROFILE").map(PathBuf::from)
            }
            #[cfg(not(windows))]
            {
                login_entry().map(|entry| entry.home)
            }
        })
}

/// The current user's home directory and login shell from the password database.
#[cfg(unix)]
struct LoginEntry {
    home: PathBuf,
    shell: String,
}

#[cfg(unix)]
fn login_entry() -> Option<LoginEntry> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt as _;

    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: the out-pointers and buffer are valid for the call; getpwuid_r is reentrant.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so the entry and the strings it points into are valid.
    let entry = unsafe { entry.assume_init() };
    let text = |pointer: *const libc::c_char| {
        (!pointer.is_null())
            // SAFETY: a non-null field of a valid entry is a terminated string.
            .then(|| unsafe { CStr::from_ptr(pointer) })
    };
    Some(LoginEntry {
        home: PathBuf::from(std::ffi::OsStr::from_bytes(text(entry.pw_dir)?.to_bytes())),
        shell: text(entry.pw_shell)
            .map(|shell| shell.to_string_lossy().into_owned())
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned()),
    })
}

/// The platform's spelling of `signal`, as `strsignal` gives it.
#[cfg(unix)]
pub fn signal_name(signal: i32) -> String {
    // SAFETY: strsignal returns null or a terminated string valid until the next call on this
    // thread, copied out at once.
    let name = unsafe { libc::strsignal(signal) };
    if name.is_null() {
        format!("Signal {signal}")
    } else {
        // SAFETY: non-null, terminated, and not retained.
        unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    }
}

/// How a signal is named where processes do not end by signal.
#[cfg(not(unix))]
pub fn signal_name(signal: i32) -> String {
    format!("Signal {signal}")
}

/// Start `spec` on a new pseudo-terminal. Its output reader stops with an error when `budget`
/// runs out (or is cancelled) before all output arrived, even while descendants keep the
/// terminal open.
///
/// # Errors
/// * When the working directory is invalid or the process cannot start.
pub fn spawn(spec: &TerminalSpawn<'_>, budget: ShellExecutionBudget) -> Result<Spawned, String> {
    platform::spawn(spec, budget)
}

mod platform {
    use std::io::{self, Read};
    use std::time::{Duration, Instant};

    use bmux_pty::{PtyCommand, PtyControl, PtyEvent, PtyExit, PtyProcess, PtySize};

    use super::{ShellExecutionBudget, TerminalChild, TerminalExit, TerminalSpawn};

    /// How often the output reader looks up its budget while no output arrives.
    const POLL: Duration = Duration::from_millis(10);

    struct PtyChild {
        control: PtyControl,
    }

    impl TerminalChild for PtyChild {
        fn try_wait(&mut self) -> io::Result<Option<TerminalExit>> {
            Ok(self.control.exit_status().map(exit))
        }

        fn terminate(&mut self) -> io::Result<()> {
            self.control.kill()
        }

        fn size(&self) -> io::Result<(u16, u16)> {
            let size = self.control.size();
            Ok((size.cols, size.rows))
        }

        fn resize(&self, columns: u16, rows: u16) -> io::Result<()> {
            self.control.resize(PtySize::new(columns, rows))
        }

        fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
            self.control.writer()?.write(bytes)
        }
    }

    fn exit(status: PtyExit) -> TerminalExit {
        // Codes are reported as the unsigned value of the same bits.
        let code = |code: i32| u32::from_ne_bytes(code.to_ne_bytes());
        TerminalExit {
            code: status.code().map_or(1, code),
            signal: status.signal().map(super::signal_name),
        }
    }

    /// The terminal output as a reader, ending at the exit once everything arrived.
    struct PtyOutput {
        process: PtyProcess,
        budget: ShellExecutionBudget,
        pending: Vec<u8>,
        offset: usize,
    }

    impl Read for PtyOutput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                if self.offset < self.pending.len() {
                    let count = buffer.len().min(self.pending.len() - self.offset);
                    buffer[..count].copy_from_slice(&self.pending[self.offset..][..count]);
                    self.offset += count;
                    return Ok(count);
                }
                if self.process.has_exited() {
                    return Ok(0);
                }
                self.budget.check().map_err(io::Error::other)?;
                match self.process.next_event_timeout(POLL) {
                    Some(PtyEvent::Output(bytes)) => {
                        self.pending = bytes;
                        self.offset = 0;
                    }
                    Some(PtyEvent::Exited(status)) if !status.output_complete() => {
                        return Err(io::Error::other(
                            "terminal output did not finish before the deadline",
                        ));
                    }
                    Some(PtyEvent::Exited(_) | PtyEvent::Stdout(_)) | None => {}
                }
            }
        }
    }

    pub fn spawn(
        spec: &TerminalSpawn<'_>,
        budget: ShellExecutionBudget,
    ) -> Result<super::Spawned, String> {
        let mut command = PtyCommand::new(spec.program).args(spec.args);
        // Without a directory, Unix commands run in the home directory and Windows ones in
        // this process's own.
        if cfg!(unix) || spec.cwd.is_some() {
            command = command.current_dir(super::working_directory(spec.cwd)?);
        }
        let mut command = command
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            // Output from descendants keeping the terminal open is awaited until the budget
            // ends; the reader enforces the budget itself.
            .drain_grace(
                budget
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_add(Duration::from_secs(1)),
            );
        #[cfg(unix)]
        if std::env::var_os("SHELL").is_none()
            && let Some(entry) = super::login_entry()
        {
            command = command.env("SHELL", entry.shell);
        }
        for name in &spec.remove_env {
            command = command.env_remove(name);
        }
        let process = command
            .spawn(PtySize::new(spec.columns, spec.rows))
            .map_err(|error| error.to_string())?;
        let child = PtyChild {
            control: process.control(),
        };
        let output = PtyOutput {
            process,
            budget,
            pending: Vec::new(),
            offset: 0,
        };
        Ok((Box::new(child), Box::new(output)))
    }
}
