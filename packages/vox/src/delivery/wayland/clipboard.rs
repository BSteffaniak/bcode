//! Native regular-selection ownership and one explicit Ctrl+V gesture.
//! No clipboard readback, history restoration, primary selection, or fallback.
use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use super::{Session, State, failure};
use crate::contracts::{Cancellation, Error, Result, TextSink};
use crate::delivery::{key_pair, prepare};

const MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];
const PASTE_KEYMAP: &str = concat!(
    "xkb_keymap { xkb_keycodes \"vox\" { minimum=8; maximum=255; <CTRL>=37; <PAST>=55; };",
    "xkb_types \"vox\" { type \"ONE_LEVEL\" { modifiers=None; map[None]=Level1; }; };",
    "xkb_compatibility \"vox\" {}; xkb_symbols \"vox\" {",
    "key <CTRL> { [ Control_L ] }; modifier_map Control { <CTRL> };",
    "key <PAST> { type=\"ONE_LEVEL\", repeat=No, [ v ] }; }; };\0"
);

/// Native regular-selection owner retained by an explicitly selected sink.
pub struct ClipboardSink {
    multiline: bool,
    owner: Option<Owner>,
}
impl ClipboardSink {
    /// Construct a sink without opening a display or changing the clipboard.
    #[must_use]
    pub const fn new(multiline: bool) -> Self {
        Self {
            multiline,
            owner: None,
        }
    }
}
impl TextSink for ClipboardSink {
    fn deliver(&mut self, text: &str, cancel: &Cancellation) -> Result<()> {
        cancel.check()?;
        let output = prepare(text, self.multiline, false)?;
        if output.is_empty() {
            return Ok(());
        }
        // Invalid or cancelled input above never replaces the existing owner.
        drop(self.owner.take());
        let stop = Cancellation::new();
        let worker_stop = stop.clone();
        let operation_cancel = cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("vox-clipboard".into())
            .spawn(move || {
                match establish(output, &operation_cancel) {
                    Ok(mut session) => {
                        if operation_cancel.is_cancelled() || worker_stop.is_cancelled() {
                            let _ = sender.send(Err(Error::Cancelled));
                            return;
                        }
                        if sender.send(Ok(())).is_err() {
                            return;
                        }
                        // This thread owns the connection and actual bytes. It
                        // services asynchronous selection requests after deliver
                        // returns, until selection replacement, cancellation, or
                        // sink drop/reuse. It never sends a second paste gesture.
                        while !worker_stop.is_cancelled() && !operation_cancel.is_cancelled() {
                            if session.sync(&worker_stop).is_err() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                    }
                }
            })
            .map_err(|_| failure("cannot start native clipboard owner"))?;
        let owner = Owner {
            stop,
            handle: Some(handle),
        };
        loop {
            if cancel.is_cancelled() {
                drop(owner);
                return Err(Error::Cancelled);
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(Ok(())) => {
                    self.owner = Some(owner);
                    return cancel.check();
                }
                Ok(Err(error)) => {
                    drop(owner);
                    return Err(error);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    drop(owner);
                    return Err(failure(
                        "native clipboard owner stopped; paste may have been attempted",
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}
struct Owner {
    stop: Cancellation,
    handle: Option<JoinHandle<()>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn establish(output: String, cancel: &Cancellation) -> Result<Session> {
    use std::os::fd::AsFd;
    let mut session = Session::connect(cancel)?;
    let manager = session.state.clipboard.manager.as_ref().ok_or_else(|| Error::Unsupported(
        "compositor does not advertise ext-data-control-v1; no clipboard or backend fallback attempted".into()
    ))?;
    if session.state.clipboard.ambiguous {
        return Err(Error::Unsupported(
            "ambiguous Wayland clipboard manager".into(),
        ));
    }
    let seat = &session
        .state
        .seats
        .values()
        .find(|(_, keyboard)| *keyboard)
        .ok_or_else(|| failure("clipboard keyboard seat disappeared"))?
        .0;
    let device = manager.get_data_device(seat, &session.queue.handle(), ());
    let source = manager.create_data_source(&session.queue.handle(), ());
    for mime in MIME_TYPES {
        source.offer(mime.into());
    }
    // Prepare and acknowledge the paste-only keyboard before changing clipboard.
    let fd = rustix::fs::memfd_create("vox-paste", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|_| failure("paste keymap allocation failed"))?;
    let mut file = File::from(fd);
    file.write_all(PASTE_KEYMAP.as_bytes())
        .map_err(|_| failure("paste keymap write failed"))?;
    session.keyboard().keymap(
        1,
        file.as_fd(),
        u32::try_from(PASTE_KEYMAP.len()).map_err(|_| failure("paste keymap too large"))?,
    );
    session.keyboard().modifiers(0, 0, 0, 0);
    session.state.clipboard.device = Some(device.clone());
    session.state.clipboard.source = Some(source.clone());
    session.state.clipboard.bytes = output.into_bytes();
    session.sync(cancel)?;
    cancel.check()?;
    device.set_selection(Some(&source));
    session.sync(cancel)?;
    if session.state.clipboard.lost {
        return Err(failure("clipboard ownership was replaced before paste"));
    }
    paste_once(&mut session, cancel)?;
    session.sync(cancel)?;
    // Service at least one complete transfer before return so a one-shot caller
    // can exit without abandoning every recipient. This can be a clipboard
    // manager: it is not proof the focused application accepted the text.
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.state.clipboard.completed == 0 {
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(failure(
                "paste gesture sent but no clipboard transfer completed; do not retry automatically",
            ));
        }
        session.sync(cancel)?;
        thread::sleep(Duration::from_millis(10));
    }
    // Acknowledges protocol processing only, not application interpretation of
    // Ctrl+V or receipt. The owner stays alive to satisfy requests arriving later.
    Ok(session)
}

trait PasteEvents: crate::delivery::KeyEvents {
    fn control(&mut self, active: bool);
}
impl PasteEvents for Session {
    fn control(&mut self, active: bool) {
        self.keyboard()
            .modifiers(if active { 4 } else { 0 }, 0, 0, 0);
    }
}
fn paste_once(events: &mut impl PasteEvents, cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    // Ctrl+V is an explicit GUI-field policy, not universal terminal paste.
    // Transcript bytes never choose keys, modifiers, or another shortcut.
    events.control(true);
    let result = key_pair(events, 47, cancel);
    events.control(false);
    result
}

#[derive(Default)]
pub(super) struct ClipboardState {
    pub(super) manager: Option<ExtDataControlManagerV1>,
    pub(super) manager_name: Option<u32>,
    ambiguous: bool,
    device: Option<ExtDataControlDeviceV1>,
    source: Option<ExtDataControlSourceV1>,
    bytes: Vec<u8>,
    transfers: Vec<Transfer>,
    completed: usize,
    lost: bool,
}
struct Transfer {
    file: File,
    offset: usize,
    deadline: Instant,
}
impl ClipboardState {
    pub(super) fn pump(&mut self) -> Result<()> {
        let bytes = &self.bytes;
        let mut completed = 0_usize;
        self.transfers.retain_mut(|transfer| {
            if Instant::now() >= transfer.deadline {
                return false;
            }
            let mut budget = 65_536;
            while transfer.offset < bytes.len() && budget > 0 {
                let end = transfer.offset.saturating_add(4096).min(bytes.len());
                match transfer.file.write(&bytes[transfer.offset..end]) {
                    Ok(0) => return false,
                    Ok(count) => {
                        transfer.offset += count;
                        budget -= count;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return true,
                    Err(_) => return false,
                }
            }
            if transfer.offset == bytes.len() {
                completed += 1;
                false
            } else {
                true
            }
        });
        self.completed = self.completed.saturating_add(completed);
        if self.lost {
            return Err(failure(
                "Wayland clipboard ownership was lost; paste is not retried",
            ));
        }
        Ok(())
    }
}

pub(super) fn discover(
    state: &mut ClipboardState,
    registry: &WlRegistry,
    name: u32,
    version: u32,
    qh: &QueueHandle<State>,
) {
    if state.manager.is_some() {
        state.ambiguous = true;
    } else if version >= 1 {
        state.manager = Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, qh, ()));
        state.manager_name = Some(name);
    }
}
wayland_client::delegate_noop!(State: ignore ExtDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Never read offers: no promise to restore old contents or history.
            ext_data_control_device_v1::Event::DataOffer { id } => id.destroy(),
            ext_data_control_device_v1::Event::Finished => state.clipboard.lost = true,
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}
impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        _: ext_data_control_offer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Cancelled => {
                state.clipboard.lost = true;
                state.clipboard.transfers.clear();
                state.clipboard.bytes.clear();
            }
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if !MIME_TYPES.contains(&mime_type.as_str())
                    || state.clipboard.lost
                    || state.clipboard.transfers.len() >= 16
                {
                    return;
                }
                if rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).is_err() {
                    return;
                }
                state.clipboard.transfers.push(Transfer {
                    file: File::from(fd),
                    offset: 0,
                    deadline: Instant::now() + Duration::from_secs(10),
                });
            }
            _ => {}
        }
    }
}

impl Drop for ClipboardState {
    fn drop(&mut self) {
        if let Some(source) = self.source.take()
            && source.is_alive()
        {
            source.destroy();
        }
        if let Some(device) = self.device.take()
            && device.is_alive()
        {
            device.destroy();
        }
        if let Some(manager) = self.manager.take()
            && manager.is_alive()
        {
            manager.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn malformed_clipboard_input_is_rejected_before_spawning_or_selection_change() {
        let mut sink = ClipboardSink::new(false);
        let error = sink
            .deliver("prefix\u{1b}[31m", &Cancellation::new())
            .expect_err("control rejected");
        assert!(matches!(error, Error::Invalid(_)));
        assert!(sink.owner.is_none());
        let cancel = Cancellation::new();
        cancel.cancel();
        assert!(matches!(
            sink.deliver("é中😀", &cancel),
            Err(Error::Cancelled)
        ));
        assert!(sink.owner.is_none());
    }

    #[test]
    fn selection_transfers_exact_utf8_and_eof() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        writer.set_nonblocking(true).expect("nonblocking writer");
        let output = "é中😀e\u{301}\nsecond line".as_bytes().to_vec();
        let mut state = ClipboardState::default();
        state.bytes.extend_from_slice(&output);
        state.transfers.push(Transfer {
            file: File::from(std::os::fd::OwnedFd::from(writer)),
            offset: 0,
            deadline: Instant::now() + Duration::from_secs(2),
        });
        state.pump().expect("transfer");
        assert!(state.transfers.is_empty());
        assert_eq!(state.completed, 1);
        let mut received = Vec::new();
        (&reader)
            .read_to_end(&mut received)
            .expect("EOF after transfer");
        assert_eq!(received, output);
    }

    #[test]
    fn expired_transfer_closes_without_output() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let mut state = ClipboardState::default();
        state.bytes.extend_from_slice(b"private");
        state.transfers.push(Transfer {
            file: File::from(std::os::fd::OwnedFd::from(writer)),
            offset: 0,
            deadline: Instant::now(),
        });
        state.pump().expect("expired transfer discarded");
        let mut received = Vec::new();
        (&reader).read_to_end(&mut received).expect("EOF");
        assert!(received.is_empty());
    }

    struct Gesture {
        events: Vec<&'static str>,
        cancel: Cancellation,
        fail_press: bool,
    }
    impl crate::delivery::KeyEvents for Gesture {
        fn press(&mut self, key: u32) -> Result<()> {
            assert_eq!(key, 47);
            self.events.push("press-v");
            self.cancel.cancel();
            if self.fail_press {
                Err(failure("injected press failure"))
            } else {
                Ok(())
            }
        }
        fn release(&mut self, key: u32) -> Result<()> {
            assert_eq!(key, 47);
            self.events.push("release-v");
            Ok(())
        }
    }
    impl PasteEvents for Gesture {
        fn control(&mut self, active: bool) {
            self.events
                .push(if active { "control-on" } else { "control-off" });
        }
    }
    #[test]
    fn cancelled_or_failed_paste_releases_v_and_control_without_retry() {
        for fail_press in [false, true] {
            let cancel = Cancellation::new();
            let mut gesture = Gesture {
                events: Vec::new(),
                cancel: cancel.clone(),
                fail_press,
            };
            assert!(paste_once(&mut gesture, &cancel).is_err());
            assert_eq!(
                gesture.events,
                ["control-on", "press-v", "release-v", "control-off"]
            );
        }
    }
    #[test]
    fn precancelled_paste_has_no_modifiers_or_key_events() {
        let cancel = Cancellation::new();
        cancel.cancel();
        let mut gesture = Gesture {
            events: Vec::new(),
            cancel: cancel.clone(),
            fail_press: false,
        };
        assert!(matches!(
            paste_once(&mut gesture, &cancel),
            Err(Error::Cancelled)
        ));
        assert!(gesture.events.is_empty());
    }
}
