//! Native text-only virtual keyboard. No subprocess, system keymap, or shortcuts.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_callback, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};

use super::{KeyEvents, key_pair, prepare};
use crate::contracts::{Cancellation, Error, Result, TextSink};

#[cfg(feature = "clipboard")]
pub(super) mod clipboard;

const CHUNK_SCALARS: usize = 200;
const ROUNDTRIP_LIMIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 20_000_000,
};

pub(super) struct WaylandSink {
    multiline: bool,
}
impl WaylandSink {
    #[must_use]
    pub(super) const fn new(multiline: bool) -> Self {
        Self { multiline }
    }
}

impl TextSink for WaylandSink {
    fn deliver(&mut self, text: &str, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        let output = prepare(text, self.multiline, true)?;
        if output.is_empty() {
            return Ok(());
        }
        let mut session = Session::connect(cancel)?;
        let scalars: Vec<char> = output.chars().collect();
        for chunk in scalars.chunks(CHUNK_SCALARS) {
            cancel.check()?;
            let map = Keymap::new(chunk);
            let fd = rustix::fs::memfd_create("vox-keymap", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|_| failure("Wayland keymap allocation failed"))?;
            let mut file = std::fs::File::from(fd);
            file.write_all(map.source.as_bytes())
                .map_err(|_| failure("Wayland keymap write failed"))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|_| failure("Wayland keymap rewind failed"))?;
            let size =
                u32::try_from(map.source.len()).map_err(|_| failure("Wayland keymap too large"))?;
            session.keyboard().keymap(1, file.as_fd(), size);
            session.keyboard().modifiers(0, 0, 0, 0);
            session.sync(cancel)?;
            for character in chunk {
                // Keycodes in XKB are evdev codes plus eight.
                let key = map
                    .codes
                    .get(character)
                    .ok_or_else(|| failure("Wayland keymap entry missing"))?
                    - 8;
                key_pair(&mut session, key, cancel)?;
            }
            // Keep the unlinked fd alive until the compositor has consumed it.
            // This acknowledges protocol processing, not application text receipt.
            session.sync(cancel)?;
        }
        cancel.check()
    }
}

fn failure(message: &str) -> Error {
    Error::Backend(message.into())
}

struct Keymap {
    source: String,
    codes: BTreeMap<char, u32>,
}
impl Keymap {
    fn new(chunk: &[char]) -> Self {
        let unique: BTreeSet<char> = chunk.iter().copied().collect();
        let codes: BTreeMap<char, u32> = unique.into_iter().zip(30_u32..).collect();
        let mut source =
            String::from("xkb_keymap {\nxkb_keycodes \"vox\" { minimum = 8; maximum = 255;\n");
        for code in codes.values() {
            // All formatted values are bounded numbers, never user syntax.
            let _ = writeln!(source, "<V{code:03}> = {code};");
        }
        source.push_str("};\nxkb_types \"vox\" { type \"ONE_LEVEL\" { modifiers = None; map[None] = Level1; level_name[Level1] = \"Any\"; }; };\nxkb_compatibility \"vox\" {};\nxkb_symbols \"vox\" {\n");
        for (character, code) in &codes {
            let value = u32::from(*character);
            let keysym = if value <= 0xff {
                value
            } else {
                0x0100_0000 | value
            };
            let _ = writeln!(
                source,
                "key <V{code:03}> {{ type[Group1] = \"ONE_LEVEL\", repeat = No, symbols[Group1] = [ 0x{keysym:08x} ] }};"
            );
        }
        source.push_str("};\n};\n\0");
        Self { source, codes }
    }
}

#[derive(Default)]
struct State {
    #[cfg(feature = "clipboard")]
    clipboard: clipboard::ClipboardState,
    manager: Option<ZwpVirtualKeyboardManagerV1>,
    manager_name: Option<u32>,
    seats: BTreeMap<u32, (wl_seat::WlSeat, bool)>,
    ambiguous: bool,
    removed: bool,
    synced: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            #[cfg(feature = "clipboard")]
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "ext_data_control_manager_v1" => {
                clipboard::discover(&mut state.clipboard, registry, name, version, qh);
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "zwp_virtual_keyboard_manager_v1" => {
                if state.manager.is_some() {
                    state.ambiguous = true;
                } else if version >= 1 {
                    state.manager =
                        Some(registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(name, 1, qh, ()));
                    state.manager_name = Some(name);
                }
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_seat" => {
                if state.seats.len() >= 16 {
                    state.ambiguous = true;
                } else if version >= 1 {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, name);
                    state.seats.insert(name, (seat, false));
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                #[cfg(feature = "clipboard")]
                if state.clipboard.manager_name == Some(name) {
                    state.removed = true;
                }
                if state.manager_name == Some(name) || state.seats.remove(&name).is_some() {
                    state.removed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
            && let Some((_, keyboard)) = state.seats.get_mut(name)
        {
            let available = capabilities.contains(wl_seat::Capability::Keyboard);
            if *keyboard && !available {
                state.removed = true;
            }
            *keyboard = available;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}
wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

struct Session {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    started: Instant,
}
impl Session {
    fn connect(cancel: &Cancellation) -> Result<Self> {
        cancel.check()?;
        let connection = Connection::connect_to_env().map_err(|_| {
            Error::Unsupported("cannot connect to the selected Wayland display".into())
        })?;
        let queue = connection.new_event_queue();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        let mut session = Self {
            connection,
            queue,
            state: State::default(),
            keyboard: None,
            started: Instant::now(),
        };
        session.sync(cancel)?;
        // Seat capabilities are events on proxies bound during registry dispatch.
        session.sync(cancel)?;
        if session.state.ambiguous {
            return Err(Error::Unsupported(
                "ambiguous Wayland manager or excessive seats".into(),
            ));
        }
        let manager = session.state.manager.as_ref().ok_or_else(|| {
            Error::Unsupported(
                "compositor does not advertise virtual-keyboard-v1; no fallback attempted".into(),
            )
        })?;
        let mut seats = session
            .state
            .seats
            .values()
            .filter(|(_, keyboard)| *keyboard);
        let (seat, _) = seats
            .next()
            .ok_or_else(|| Error::Unsupported("Wayland has no keyboard-capable seat".into()))?;
        if seats.next().is_some() {
            return Err(Error::Unsupported(
                "multiple Wayland keyboard seats require an explicit seat policy".into(),
            ));
        }
        session.keyboard = Some(manager.create_virtual_keyboard(seat, &session.queue.handle(), ()));
        session.sync(cancel)?;
        Ok(session)
    }

    const fn keyboard(&self) -> &ZwpVirtualKeyboardV1 {
        // Only called after successful connect and before Drop takes ownership.
        self.keyboard.as_ref().expect("connected virtual keyboard")
    }

    fn sync(&mut self, cancel: &Cancellation) -> Result<()> {
        self.state.synced = false;
        let _callback = self.connection.display().sync(&self.queue.handle(), ());
        let deadline = Instant::now() + ROUNDTRIP_LIMIT;
        loop {
            cancel.check()?;
            if Instant::now() >= deadline {
                return Err(failure(
                    "Wayland roundtrip timed out; delivery may be partial",
                ));
            }
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(|_| failure("Wayland dispatch failed; delivery may be partial"))?;
            #[cfg(feature = "clipboard")]
            self.state.clipboard.pump()?;
            if self.state.removed {
                return Err(failure(
                    "Wayland delivery capability disappeared; delivery may be partial",
                ));
            }
            let blocked = match self.connection.flush() {
                Ok(()) => false,
                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    true
                }
                Err(_) => {
                    return Err(failure(
                        "Wayland connection failed; delivery may be partial",
                    ));
                }
            };
            if self.state.synced && !blocked {
                return Ok(());
            }
            let Some(guard) = self.connection.prepare_read() else {
                continue;
            };
            let flags = PollFlags::IN
                | if blocked {
                    PollFlags::OUT
                } else {
                    PollFlags::empty()
                };
            let mut fds = [PollFd::new(&self.connection, flags)];
            match poll(&mut fds, Some(&POLL_INTERVAL)) {
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(failure("Wayland socket polling failed")),
                Ok(_) => {}
            }
            let ready = fds[0].revents();
            if ready.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                return Err(failure("Wayland socket closed; delivery may be partial"));
            }
            if ready.contains(PollFlags::IN) {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        return Err(failure(
                            "Wayland socket read failed; delivery may be partial",
                        ));
                    }
                }
            }
            // Without readable data, dropping the guard cancels the prepared read.
        }
    }
}

impl KeyEvents for Session {
    fn press(&mut self, key: u32) -> Result<()> {
        let time = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.keyboard().key(time, key, 1);
        Ok(())
    }
    fn release(&mut self, key: u32) -> Result<()> {
        let time = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.keyboard().key(time, key, 0);
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(keyboard) = self.keyboard.take() {
            // Every queued press is paired before cancellation can be observed.
            // Destruction also removes this virtual device on disconnect. Cleanup
            // is best effort, nonblocking, and never repeats text events.
            keyboard.modifiers(0, 0, 0, 0);
            keyboard.destroy();
            let _ = self.connection.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_scalars_receive_distinct_bounded_keycodes() {
        let text: Vec<char> = "é中😀e\u{301}👩\u{200d}💻".chars().collect();
        let map = Keymap::new(&text);
        assert_eq!(
            map.codes.len(),
            text.iter().copied().collect::<BTreeSet<_>>().len()
        );
        assert!(map.codes.values().all(|code| (8..=255).contains(code)));
        // Decode the actual generated symbol values, not its formatting or names.
        let symbols: BTreeSet<u32> = map
            .source
            .split("[ 0x")
            .skip(1)
            .map(|part| u32::from_str_radix(&part[..8], 16).expect("numeric keysym"))
            .collect();
        for character in text {
            let value = u32::from(character);
            assert!(symbols.contains(
                &(if value <= 255 {
                    value
                } else {
                    value | 0x0100_0000
                })
            ));
        }
        assert_eq!(map.source.as_bytes().last(), Some(&0));
    }

    #[test]
    fn chunk_capacity_stays_inside_evdev_keycode_range() {
        let text: Vec<char> = (0x4e00
            ..0x4e00 + u32::try_from(CHUNK_SCALARS).expect("bounded chunk"))
            .filter_map(char::from_u32)
            .collect();
        let map = Keymap::new(&text);
        assert_eq!(map.codes.len(), CHUNK_SCALARS);
        assert!(map.codes.values().all(|code| *code >= 8 && *code <= 255));
    }
}
