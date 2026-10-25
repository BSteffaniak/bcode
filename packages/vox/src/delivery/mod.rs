//! Explicit final-text delivery. No backend retries or clipboard fallback.
//!
//! Typing never emits Enter, Tab, modifiers, or other command keys. Newlines are
//! flattened by default; multiline typing fails closed. Explicit Wayland clipboard
//! mode owns the regular selection and sends one Ctrl+V gesture. Host delivery
//! must be supplied by the embedding application through its own `TextSink`.

use std::io::Write;

use crate::config::Config;
use crate::contracts::{Cancellation, DeliveryMode, Error, Result, TextSink};

#[cfg(all(target_os = "linux", feature = "wayland"))]
mod wayland;
#[cfg(all(target_os = "linux", feature = "x11"))]
mod x11;

#[cfg(test)]
mod tests;

const MAX_TEXT_BYTES: usize = 1_048_576;

/// Construct the explicitly configured sink without delivering any text.
///
/// `auto` selects Wayland when its environment is present, otherwise X11. A
/// selected but unavailable backend is an error, never a reason to try another
/// backend. Desktop connections are opened only by `deliver`, after cancellation
/// and whole-input validation. No focus is captured or restored.
///
/// # Errors
/// Returns `Error::Invalid` for unknown backend selections and
/// `Error::Unsupported` for missing compiled capabilities, absent desktop
/// selections, or standalone host delivery. Desktop authorization/capability
/// errors surface when delivering. Any delivery error may indicate partial output;
/// callers must not retry that output automatically.
pub fn sink(config: &Config) -> Result<Box<dyn TextSink>> {
    match config.delivery {
        DeliveryMode::Stdout => Ok(Box::new(StdoutSink {
            multiline: config.multiline,
        })),
        DeliveryMode::Clipboard => clipboard_sink(config),
        DeliveryMode::Host => Err(Error::Unsupported(
            "host delivery requires an embedding-provided TextSink".into(),
        )),
        DeliveryMode::Typing => {
            let backend = select_backend(
                &config.delivery_backend,
                std::env::var_os("WAYLAND_DISPLAY").is_some()
                    || std::env::var_os("WAYLAND_SOCKET").is_some(),
                std::env::var_os("DISPLAY").is_some(),
            )?;
            typing_sink(backend, config.multiline)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Wayland,
    X11,
}

fn select_backend(name: &str, wayland: bool, x11: bool) -> Result<Backend> {
    match name {
        "wayland" => Ok(Backend::Wayland),
        "x11" => Ok(Backend::X11),
        "auto" if wayland => Ok(Backend::Wayland),
        "auto" if x11 => Ok(Backend::X11),
        "auto" => Err(Error::Unsupported(
            "no Linux desktop display configured".into(),
        )),
        _ => Err(Error::Invalid(
            "delivery_backend must be auto, wayland or x11".into(),
        )),
    }
}

fn typing_sink(backend: Backend, multiline: bool) -> Result<Box<dyn TextSink>> {
    let available = match backend {
        Backend::Wayland => cfg!(all(target_os = "linux", feature = "wayland")),
        Backend::X11 => cfg!(all(target_os = "linux", feature = "x11")),
    };
    if !available {
        return Err(Error::Unsupported(
            "selected typing backend is not compiled for this platform".into(),
        ));
    }
    match backend {
        Backend::Wayland => {
            #[cfg(all(target_os = "linux", feature = "wayland"))]
            {
                Ok(Box::new(wayland::WaylandSink::new(multiline)))
            }
            #[cfg(not(all(target_os = "linux", feature = "wayland")))]
            {
                let _ = multiline;
                Err(Error::Unsupported(
                    "Wayland typing requires Linux and the wayland feature".into(),
                ))
            }
        }
        Backend::X11 => {
            #[cfg(all(target_os = "linux", feature = "x11"))]
            {
                Ok(Box::new(x11::X11Sink::new(multiline)))
            }
            #[cfg(not(all(target_os = "linux", feature = "x11")))]
            {
                let _ = multiline;
                Err(Error::Unsupported(
                    "X11 typing requires Linux and the x11 feature".into(),
                ))
            }
        }
    }
}

// Validate the entire input before any external effect, including stdout. Do not
// interpolate input or backend errors into diagnostics.
fn prepare(text: &str, multiline: bool, typing: bool) -> Result<String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(Error::Invalid(
            "delivery text exceeds the 1 MiB limit".into(),
        ));
    }
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if matches!(character, '\r' | '\n' | '\u{2028}' | '\u{2029}') {
            if character == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            if multiline && typing {
                return Err(Error::Unsupported(
                    "multiline typing cannot safely inject Enter; select stdout or clipboard"
                        .into(),
                ));
            }
            output.push(if multiline { '\n' } else { ' ' });
        } else if character.is_control() {
            return Err(Error::Invalid(
                "delivery text contains a forbidden control character".into(),
            ));
        } else {
            output.push(character);
        }
    }
    Ok(output)
}

struct StdoutSink {
    multiline: bool,
}

impl TextSink for StdoutSink {
    fn deliver(&mut self, text: &str, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        let output = prepare(text, self.multiline, false)?;
        let stdout = std::io::stdout();
        write_output(&mut stdout.lock(), output.as_bytes(), cancel)
    }
}

fn write_output(writer: &mut impl Write, output: &[u8], cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    // No added newline, framing, status messages, or retry after a write error.
    for chunk in output.chunks(4096) {
        cancel.check()?;
        writer
            .write_all(chunk)
            .map_err(|_| Error::Backend("stdout delivery failed; output may be partial".into()))?;
    }
    writer
        .flush()
        .map_err(|_| Error::Backend("stdout flush failed; output may be partial".into()))?;
    cancel.check()
}

fn clipboard_sink(config: &Config) -> Result<Box<dyn TextSink>> {
    #[cfg(all(target_os = "linux", feature = "clipboard", feature = "wayland"))]
    {
        let backend = select_backend(
            &config.delivery_backend,
            std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var_os("WAYLAND_SOCKET").is_some(),
            std::env::var_os("DISPLAY").is_some(),
        )?;
        if backend == Backend::Wayland {
            return Ok(Box::new(wayland::clipboard::ClipboardSink::new(
                config.multiline,
            )));
        }
    }
    let _ = config;
    Err(Error::Unsupported(
        "clipboard paste requires Linux, clipboard+wayland features, and a Wayland backend; no copy-only or backend fallback attempted".into(),
    ))
}

// A press attempt always has exactly one matching release attempt, even if the
// press fails or cancellation arrives in between. Never retry a press.
#[cfg(any(
    test,
    all(target_os = "linux", any(feature = "wayland", feature = "x11"))
))]
trait KeyEvents {
    fn press(&mut self, key: u32) -> Result<()>;
    fn release(&mut self, key: u32) -> Result<()>;
}

#[cfg(any(
    test,
    all(target_os = "linux", any(feature = "wayland", feature = "x11"))
))]
fn key_pair(events: &mut impl KeyEvents, key: u32, cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    let pressed = events.press(key);
    let released = events.release(key);
    pressed?;
    released?;
    cancel.check()
}
