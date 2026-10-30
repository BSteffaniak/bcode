//! Generic bounded diagnostic callback leases, independent of invocation stack callbacks.
//! Plugin data never chooses authorization, session, or originating correlation.

#[cfg(test)]
mod tests;

use bcode_plugin_sdk::diagnostics::{
    DiagnosticControl, DiagnosticControlOperation, DiagnosticStatus, MAX_NATIVE_DIAGNOSTIC_BYTES,
    NATIVE_DIAGNOSTICS_VERSION, NativeDiagnosticContext, NativeDiagnosticEvent,
    NativeDiagnosticMode,
};
use bcode_xray::CaptureContext;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_LEASES: usize = 1024;
struct Entry {
    key: usize,
    capture: CaptureContext,
    owners: AtomicUsize,
}
struct Slot {
    entry: Mutex<Option<Arc<Entry>>>,
    key: AtomicUsize,
    origin: AtomicPtr<Entry>,
    readers: AtomicUsize,
}
static LEASES: OnceLock<[Slot; MAX_LEASES]> = OnceLock::new();
fn slot(key: usize) -> &'static Slot {
    &LEASES.get_or_init(|| {
        std::array::from_fn(|_| Slot {
            entry: Mutex::new(None),
            key: AtomicUsize::new(0),
            origin: AtomicPtr::new(std::ptr::null_mut()),
            readers: AtomicUsize::new(0),
        })
    })[key % MAX_LEASES]
}

/// Origin captured before executor dispatch, without secret-bearing Debug output.
#[derive(Clone, Default)]
pub struct DiagnosticOrigin(pub Option<CaptureContext>);
impl std::fmt::Debug for DiagnosticOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagnosticOrigin")
            .field("available", &self.0.is_some())
            .finish()
    }
}
impl DiagnosticOrigin {
    pub fn current() -> Self {
        Self(CaptureContext::current())
    }
}

/// Base registration ends with the FFI call. SDK sinks acquire an independent diagnostic
/// lease. Leaked sinks consume bounded slots, never invocation stack or ordinary callbacks.
pub struct DiagnosticRegistration(usize);
impl DiagnosticRegistration {
    pub fn new(handle: *mut c_void, capture: Option<CaptureContext>) -> Option<Self> {
        let capture = capture?;
        if !capture.capture_active() {
            return None;
        }
        let key = handle as usize;
        if key == 0 {
            return None;
        }
        let slot = slot(key);
        let Ok(mut value) = slot.entry.try_lock() else {
            capture.emit(
                "plugin",
                "diagnostic_channel_unavailable",
                serde_json::json!({"reason":"busy"}),
                false,
            );
            return None;
        };
        if value.is_some() {
            drop(value);
            capture.emit(
                "plugin",
                "diagnostic_channel_unavailable",
                serde_json::json!({"reason":"capacity"}),
                false,
            );
            return None;
        }
        let entry = Arc::new(Entry {
            key,
            capture,
            owners: AtomicUsize::new(1),
        });
        slot.origin.store(
            Arc::into_raw(Arc::clone(&entry)).cast_mut(),
            Ordering::SeqCst,
        );
        *value = Some(entry);
        slot.key.store(key, Ordering::Release);
        Some(Self(key))
    }
    pub fn negotiation(&self) -> Option<NativeDiagnosticContext> {
        let entry = lookup(self.0)?;
        Some(NativeDiagnosticContext {
            version: NATIVE_DIAGNOSTICS_VERSION,
            mode: NativeDiagnosticMode::CallbackV1,
            correlation: entry.capture.correlation().clone(),
            max_event_bytes: MAX_NATIVE_DIAGNOSTIC_BYTES,
        })
    }
}
impl Drop for DiagnosticRegistration {
    fn drop(&mut self) {
        release(self.0);
    }
}

/// Whether this callback handle negotiated the reserved discriminators. Old invocations
/// retain byte-for-byte ordinary callback semantics even for payloads using these prefixes.
pub fn negotiated(key: usize) -> bool {
    key != 0 && slot(key).key.load(Ordering::Acquire) == key
}

fn lookup(key: usize) -> Option<Arc<Entry>> {
    if key == 0 {
        return None;
    }
    let slot = slot(key);
    if slot.key.load(Ordering::Acquire) != key {
        return None;
    }
    let Ok(value) = slot.entry.try_lock() else {
        record_loss(key, "callback_registry_contention");
        return None;
    };
    value.as_ref().filter(|entry| entry.key == key).cloned()
}

fn origin(key: usize) -> Option<Arc<Entry>> {
    if key == 0 {
        return None;
    }
    let slot = slot(key);
    slot.readers.fetch_add(1, Ordering::SeqCst);
    let pointer = slot.origin.load(Ordering::SeqCst);
    let result = if pointer.is_null() {
        None
    } else {
        // SAFETY: final release unpublishes and waits for readers before reclaiming
        // the owning raw Arc. No invocation stack address is retained or borrowed.
        unsafe {
            Arc::increment_strong_count(pointer);
            Some(Arc::from_raw(pointer))
        }
    };
    slot.readers.fetch_sub(1, Ordering::SeqCst);
    result.filter(|entry| entry.key == key)
}

pub fn record_loss(key: usize, reason: &'static str) {
    if let Some(entry) = origin(key) {
        entry.capture.emit(
            "plugin",
            "diagnostic_bridge_gap",
            serde_json::json!({
                "reason":reason, "unavailable_callbacks":1, "complete":false,
            }),
            false,
        );
    }
}

fn release(key: usize) {
    if key == 0 {
        return;
    }
    let slot = slot(key);
    // Cleanup is not a producer operation. No callbacks or capture code run under this lock.
    let removed = slot.entry.lock().map_or_else(
        |_| {
            record_loss(key, "lease_release_registry_poisoned");
            None
        },
        |mut value| {
            if value.as_ref().is_some_and(|entry| {
                entry.key == key && entry.owners.fetch_sub(1, Ordering::AcqRel) == 1
            }) {
                let pointer = slot.origin.swap(std::ptr::null_mut(), Ordering::SeqCst);
                slot.key.store(0, Ordering::Release);
                // Cleanup may wait; producers never do. Counted readers only clone an Arc,
                // never execute callbacks, so nested upstream release cannot deadlock.
                while slot.readers.load(Ordering::SeqCst) != 0 {
                    std::thread::yield_now();
                }
                let published = if pointer.is_null() {
                    None
                } else {
                    // SAFETY: unique raw owning reference, unpublished with readers retired.
                    Some(unsafe { Arc::from_raw(pointer) })
                };
                value.take().map(|entry| (entry, published))
            } else {
                None
            }
        },
    );
    // Destroying an external context may release an upstream lease during nested service calls.
    // It must never happen under this registry's lock.
    drop(removed);
}

/// Reserved controls are intercepted before normal bridge/cancellation dispatch.
pub fn control(key: usize, bytes: &[u8]) -> Option<DiagnosticStatus> {
    if bytes.len() > 256 {
        record_loss(key, "oversized_control");
        return None;
    }
    let Ok(request) = serde_json::from_slice::<DiagnosticControl>(bytes) else {
        record_loss(key, "malformed_control");
        return None;
    };
    if request.version != NATIVE_DIAGNOSTICS_VERSION {
        record_loss(key, "unsupported_control_version");
        return None;
    }
    if matches!(request.operation, DiagnosticControlOperation::Release) {
        release(key);
        return Some(DiagnosticStatus {
            version: NATIVE_DIAGNOSTICS_VERSION,
            ..Default::default()
        });
    }
    // Acquire must increment while holding the slot lock to serialize with final release.
    let entry = if matches!(request.operation, DiagnosticControlOperation::Acquire) {
        let Ok(value) = slot(key).entry.try_lock() else {
            record_loss(key, "lease_acquire_contention");
            return None;
        };
        let entry = value.as_ref().filter(|entry| entry.key == key)?.clone();
        if entry
            .owners
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_LEASES).then_some(n + 1)
            })
            .is_err()
        {
            drop(value);
            record_loss(key, "lease_owner_capacity");
            return None;
        }
        entry
    } else {
        lookup(key)?
    };
    // Arbitrary plugin JSON cannot be certified secret-safe by its own boolean flag.
    let deep_active = entry.capture.deep_capture_active();
    Some(DiagnosticStatus {
        version: NATIVE_DIAGNOSTICS_VERSION,
        active: deep_active,
        deep_active,
    })
}

/// Bounded, nonblocking producer. It never forwards payloads to ordinary service consumers.
pub fn event(key: usize, bytes: &[u8]) {
    let Some(entry) = lookup(key) else { return };
    if !entry.capture.deep_capture_active() {
        return;
    }
    if bytes.len() > MAX_NATIVE_DIAGNOSTIC_BYTES {
        entry.capture.emit(
            "plugin",
            "diagnostic_payload_omitted",
            serde_json::json!({
                "reason":"callback_budget_exceeded", "complete":false,
            }),
            false,
        );
        return;
    }
    let Ok(envelope) = serde_json::from_slice::<NativeDiagnosticEvent>(bytes) else {
        entry.capture.emit(
            "plugin",
            "diagnostic_payload_omitted",
            serde_json::json!({
                "reason":"invalid_callback_envelope", "complete":false,
            }),
            false,
        );
        return;
    };
    if envelope.version != NATIVE_DIAGNOSTICS_VERSION {
        entry.capture.emit(
            "plugin",
            "diagnostic_payload_omitted",
            serde_json::json!({
                "reason":"unsupported_callback_version", "complete":false,
            }),
            false,
        );
        return;
    }
    let event = envelope.event;
    // NEVER use plugin correlation or sensitivity as authority. Every plugin-authored body
    // is protected; only the originating host context supplies scope and authorization.
    entry
        .capture
        .emit(&event.domain, &event.name, event.data, true);
}
