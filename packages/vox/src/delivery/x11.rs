//! Conservative XTEST adapter using only the active unmodified keymap.
//!
//! Never remaps the server keyboard, emits modifiers, or falls back to clipboard.
//! Input not representable by an unmodified key is rejected before the first
//! event. This intentionally has less Unicode coverage than the Wayland sink.
use std::collections::BTreeMap;
use std::io::{self, IoSlice};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xkb::{ConnectionExt as _, ID};
use x11rb::protocol::xproto::{ConnectionExt as _, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use super::{KeyEvents, key_pair, prepare};
use crate::contracts::{Cancellation, Error, Result, TextSink};

pub(super) struct X11Sink {
    multiline: bool,
}
impl X11Sink {
    #[must_use]
    pub(super) const fn new(multiline: bool) -> Self {
        Self { multiline }
    }
}
impl TextSink for X11Sink {
    fn deliver(&mut self, text: &str, cancel: &Cancellation) -> Result<()> {
        self.deliver_once(text, cancel).map_err(|error| {
            if cancel.is_cancelled() {
                Error::Cancelled
            } else {
                error
            }
        })
    }
}
impl X11Sink {
    fn deliver_once(&self, text: &str, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        let output = prepare(text, self.multiline, true)?;
        if output.is_empty() {
            return Ok(());
        }
        let connection = connect(cancel)?;
        cancel.check()?;
        connection
            .xtest_get_version(2, 2)
            .map_err(|_| failure())?
            .reply()
            .map_err(|_| Error::Unsupported("X11 server does not provide XTEST".into()))?;
        let xkb = connection
            .xkb_use_extension(1, 0)
            .map_err(|_| failure())?
            .reply()
            .map_err(|_| failure())?;
        if !xkb.supported {
            return Err(Error::Unsupported(
                "X11 text safety requires XKB state discovery".into(),
            ));
        }
        let setup = connection.setup();
        let first = setup.min_keycode;
        let count = setup
            .max_keycode
            .checked_sub(first)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(failure)?;
        cancel.check()?;
        let mapping = connection
            .get_keyboard_mapping(first, count)
            .map_err(|_| failure())?
            .reply()
            .map_err(|_| failure())?;
        let codes = unmodified_codes(first, mapping.keysyms_per_keycode, &mapping.keysyms)?;
        // Whole-text preflight prevents unsupported characters late in the text
        // from producing a known partial delivery.
        let keys: Vec<u32> = output.chars().map(|character| {
            codes.get(&keysym(character)).copied().map(u32::from)
                .ok_or_else(|| Error::Unsupported("text is not representable by unmodified X11 keys; use Wayland or explicit clipboard".into()))
        }).collect::<Result<_>>()?;
        let mut session = Session {
            connection,
            pending_press: None,
        };
        for key in keys {
            cancel.check()?;
            // Fail rather than turn literal text into a user's held-key chord.
            // A physical key may still change between this query and injection;
            // X11 cannot provide an atomic text-only injection guarantee.
            let held = session
                .connection
                .query_keymap()
                .map_err(|_| failure())?
                .reply()
                .map_err(|_| failure())?;
            if held.keys.iter().any(|value| *value != 0) {
                return Err(Error::Unsupported(
                    "physical X11 keys are held; text delivery stopped without retry".into(),
                ));
            }
            // Reject all latched/locked modifiers and non-default groups rather
            // than guessing what symbol the server will resolve.
            let state = session
                .connection
                .xkb_get_state(ID::USE_CORE_KBD.into())
                .map_err(|_| failure())?
                .reply()
                .map_err(|_| failure())?;
            if u16::from(state.mods) != 0
                || u16::from(state.latched_mods) != 0
                || u16::from(state.locked_mods) != 0
                || u8::from(state.group) != 0
            {
                return Err(Error::Unsupported("X11 modifiers or alternate keyboard groups are active; text delivery stopped without retry".into()));
            }
            key_pair(&mut session, key, cancel)?;
        }
        cancel.check()
    }
}

fn failure() -> Error {
    Error::Backend(
        "X11 delivery failed; output may be partial and must not be retried automatically".into(),
    )
}

const fn keysym(character: char) -> u32 {
    let value = character as u32;
    if value <= 255 {
        value
    } else {
        value | 0x0100_0000
    }
}

fn unmodified_codes(first: u8, width: u8, symbols: &[u32]) -> Result<BTreeMap<u32, u8>> {
    if width == 0 || !symbols.len().is_multiple_of(usize::from(width)) {
        return Err(failure());
    }
    let mut codes = BTreeMap::new();
    for (offset, row) in symbols.chunks(usize::from(width)).enumerate() {
        let code = first
            .checked_add(u8::try_from(offset).map_err(|_| failure())?)
            .ok_or_else(failure)?;
        if row[0] != 0 {
            codes.entry(row[0]).or_insert(code);
        }
    }
    Ok(codes)
}

struct Session {
    connection: RustConnection<BoundedStream>,
    pending_press: Option<u64>,
}
impl KeyEvents for Session {
    fn press(&mut self, key: u32) -> Result<()> {
        let code = u8::try_from(key).map_err(|_| failure())?;
        self.connection
            .stream()
            .cleanup
            .store(true, Ordering::Release);
        let cookie = self
            .connection
            .xtest_fake_input(
                KEY_PRESS_EVENT,
                code,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )
            .map_err(|_| failure())?;
        self.pending_press = Some(cookie.sequence_number());
        // Do not wait for a reply with a key held down. The release is queued
        // next, before either request is checked or cancellation is observed.
        std::mem::forget(cookie);
        Ok(())
    }
    fn release(&mut self, key: u32) -> Result<()> {
        let result = self.release_checked(key);
        self.connection
            .stream()
            .cleanup
            .store(false, Ordering::Release);
        result
    }
}
impl Session {
    fn release_checked(&mut self, key: u32) -> Result<()> {
        let code = u8::try_from(key).map_err(|_| failure())?;
        let release = self
            .connection
            .xtest_fake_input(
                KEY_RELEASE_EVENT,
                code,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            )
            .map_err(|_| failure())?;
        let pressed = self.pending_press.take();
        release.check().map_err(|_| failure())?;
        if let Some(sequence) = pressed {
            self.connection
                .check_for_error(sequence)
                .map_err(|_| failure())?;
        }
        Ok(())
    }
}

// Restrict desktop injection to a local Unix display. No network display or
// hostname resolution, and no second address/backend after a failed attempt.
fn connect(cancel: &Cancellation) -> Result<RustConnection<BoundedStream>> {
    cancel.check()?;
    let display = parse_display::parse_display(None)
        .map_err(|_| Error::Unsupported("invalid X11 display selection".into()))?;
    let address = display
        .connect_instruction()
        .next()
        .ok_or_else(|| Error::Unsupported("X11 display has no local address".into()))?;
    if !matches!(address, parse_display::ConnectAddress::Socket(_)) {
        return Err(Error::Unsupported(
            "Vox X11 typing supports local Unix displays only".into(),
        ));
    }
    let (stream, (family, peer)) = DefaultStream::connect(&address).map_err(|_| {
        Error::Unsupported("cannot connect to the selected local X11 display".into())
    })?;
    let auth = xauth::get_auth(family, &peer, display.display)
        .map_err(|_| Error::Unsupported("cannot read X11 authorization".into()))?;
    let (name, data) = auth.unwrap_or_default();
    let bounded = BoundedStream {
        stream,
        cancel: cancel.clone(),
        cleanup: AtomicBool::new(false),
        deadline: Instant::now() + Duration::from_secs(30),
    };
    RustConnection::connect_to_stream_with_auth_info(
        bounded,
        usize::from(display.screen),
        name,
        data,
    )
    .map_err(|_| {
        if cancel.is_cancelled() {
            Error::Cancelled
        } else {
            failure()
        }
    })
}

// x11rb's nonblocking transport extension keeps handshake and reply waits
// cancellable without background threads that could keep typing after return.
// A fixed 30-second operation budget bounds even a continuously chatty peer.
struct BoundedStream {
    stream: DefaultStream,
    cancel: Cancellation,
    cleanup: AtomicBool,
    deadline: Instant,
}
impl BoundedStream {
    fn check(&self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "X11 delivery deadline exceeded",
            ));
        }
        if !self.cleanup.load(Ordering::Acquire) && self.cancel.is_cancelled() {
            return Err(io::Error::other("X11 delivery cancelled"));
        }
        Ok(())
    }
}
impl Stream for BoundedStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::IN;
        }
        if mode.writable() {
            flags |= PollFlags::OUT;
        }
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        loop {
            self.check()?;
            let mut fds = [PollFd::new(&self.stream, flags)];
            match poll(&mut fds, Some(&timeout)) {
                Err(rustix::io::Errno::INTR) | Ok(0) => {}
                Err(error) => return Err(error.into()),
                Ok(_) => return Ok(()),
            }
        }
    }
    fn read(&self, bytes: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.stream.read(bytes, fds)
    }
    fn write(&self, bytes: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        self.stream.write(bytes, fds)
    }
    fn write_vectored(
        &self,
        bytes: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.check()?;
        self.stream.write_vectored(bytes, fds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unmodified_group_one_is_selected() {
        let codes = unmodified_codes(8, 4, &[0x61, 0x41, 0x62, 0x42, 0x0100_4e2d, 0, 0, 0])
            .expect("valid mapping");
        assert_eq!(codes.get(&keysym('a')), Some(&8));
        assert_eq!(codes.get(&keysym('中')), Some(&9));
        assert!(!codes.contains_key(&keysym('A')));
        assert!(!codes.contains_key(&keysym('b')));
    }

    #[test]
    fn malformed_and_overflowing_maps_fail_closed() {
        assert!(unmodified_codes(8, 0, &[]).is_err());
        assert!(unmodified_codes(8, 2, &[1]).is_err());
        assert!(unmodified_codes(255, 1, &[1, 2]).is_err());
    }

    #[test]
    fn unicode_keysym_conversion_preserves_scalars() {
        assert_eq!(keysym('é'), 0xe9);
        assert_eq!(keysym('中'), 0x0100_4e2d);
        assert_eq!(keysym('😀'), 0x0101_f600);
        assert_eq!(keysym('\u{301}'), 0x0100_0301);
    }
}
