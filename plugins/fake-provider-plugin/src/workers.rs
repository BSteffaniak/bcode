//! Instance-owned fake-provider worker admission and drain.
//!
//! Closing and admission share one lock. A worker reserves its slot before it
//! can run; completion and the drain observer use the same condition variable.
//! A timed-out observer does not discard the registry or authorize unloading.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use bcode_plugin_sdk::PluginError;

#[derive(Debug, Default)]
pub struct Workers {
    state: Mutex<State>,
    drained: Condvar,
    drain_observer: Mutex<()>,
}

#[derive(Debug, Default)]
struct State {
    closing: bool,
    active: usize,
    handles: Vec<std::thread::JoinHandle<()>>,
}

struct Completion(Arc<Workers>);

impl Drop for Completion {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        if state.active == 0 {
            self.0.drained.notify_all();
        }
    }
}

impl Workers {
    pub fn spawn(self: &Arc<Self>, work: impl FnOnce() + Send + 'static) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closing || state.active >= 256 {
            return false;
        }
        state.active += 1;
        let completion = Completion(Arc::clone(self));
        drop(state);
        // Do not allow completion before its join handle is owned by the instance.
        // No registry lock is held across spawning: failure drops Completion.
        let (start, started) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("bcode-fake-provider".into())
            .spawn(move || {
                let _completion = completion;
                if started.recv().is_ok() {
                    work();
                }
            });
        let Ok(handle) = spawned else {
            return false;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Reap completed workers during normal admission to bound retained handles.
        let mut index = 0;
        while index < state.handles.len() {
            if state.handles[index].is_finished() {
                let _ = state.handles.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        state.handles.push(handle);
        drop(state);
        start.send(()).is_ok()
    }

    pub fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closing = true;
    }

    pub fn drain(&self) -> Result<(), PluginError> {
        let _observer = self
            .drain_observer
            .lock()
            .map_err(|_| PluginError::failed("fake worker drain observer unavailable"))?;
        let handles = {
            let mut state = self
                .drained
                .wait_timeout_while(
                    self.state
                        .lock()
                        .map_err(|_| PluginError::failed("fake worker state unavailable"))?,
                    Duration::from_secs(5),
                    |state| state.active != 0,
                )
                .map_err(|_| PluginError::failed("fake worker drain unavailable"))?
                .0;
            if state.active != 0 {
                return Err(PluginError::failed(
                    "fake workers are still draining; retain instance and retry cleanup",
                ));
            }
            let handles = std::mem::take(&mut state.handles);
            drop(state);
            handles
        };
        // Completion alone is insufficient for native unload: join the actual
        // OS threads, including closure destructors and unwinding epilogues.
        for handle in handles {
            let _ = handle.join();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    #[test]
    fn close_prevents_new_workers_and_drains_admitted_worker() {
        let workers = Arc::new(Workers::default());
        let (started, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        workers.spawn(move || {
            started.send(()).expect("worker ready");
            released.recv().expect("release worker");
        });
        ready.recv().expect("worker started");
        workers.close();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        workers.spawn(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        release.send(()).expect("release admitted worker");
        workers.drain().expect("worker drained");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn instances_have_independent_admission() {
        let closed = Arc::new(Workers::default());
        let active = Arc::new(Workers::default());
        closed.close();
        let (sent, received) = mpsc::channel();
        active.spawn(move || {
            sent.send(()).expect("independent worker");
        });
        received.recv().expect("other instance still admits work");
        active.close();
        active.drain().expect("drain independent instance");
    }
}
