//! Atomic exclusion between new operation admission and prepared shutdown.
//!
//! Permits cover registration, not just cancellation delivery. A host must retain each
//! permit until work settles or has been transferred into a separately checked blocker.

use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPhase {
    Running,
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub generation: u64,
    pub active_operations: usize,
    pub phase: AdmissionPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionBlocked {
    Busy(AdmissionSnapshot),
    Unavailable,
}

#[derive(Debug)]
pub struct ShutdownAdmissionGate {
    state: Mutex<AdmissionSnapshot>,
    changes: tokio::sync::watch::Sender<u64>,
}

impl Default for ShutdownAdmissionGate {
    fn default() -> Self {
        Self {
            changes: tokio::sync::watch::channel(0).0,
            state: Mutex::new(AdmissionSnapshot {
                generation: 0,
                active_operations: 0,
                phase: AdmissionPhase::Running,
            }),
        }
    }
}

impl AdmissionSnapshot {
    const fn advance(&mut self) -> Result<(), AdmissionBlocked> {
        let Some(generation) = self.generation.checked_add(1) else {
            self.phase = AdmissionPhase::Committed;
            return Err(AdmissionBlocked::Unavailable);
        };
        self.generation = generation;
        Ok(())
    }
}

impl ShutdownAdmissionGate {
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn snapshot(&self) -> Result<AdmissionSnapshot, AdmissionBlocked> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| AdmissionBlocked::Unavailable)
    }

    pub fn admit(&self) -> Result<OperationAdmission<'_>, AdmissionBlocked> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AdmissionBlocked::Unavailable)?;
        if state.phase != AdmissionPhase::Running {
            return Err(AdmissionBlocked::Busy(*state));
        }
        let Some(active) = state.active_operations.checked_add(1) else {
            state.phase = AdmissionPhase::Committed;
            return Err(AdmissionBlocked::Unavailable);
        };
        state.advance()?;
        state.active_operations = active;
        self.changes.send_replace(state.generation);
        drop(state);
        Ok(OperationAdmission { gate: self })
    }

    pub fn prepare_observed(
        &self,
        generation: u64,
    ) -> Result<ShutdownReservation<'_>, AdmissionBlocked> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AdmissionBlocked::Unavailable)?;
        if state.generation != generation
            || state.phase != AdmissionPhase::Running
            || state.active_operations != 0
        {
            return Err(AdmissionBlocked::Busy(*state));
        }
        state.advance()?;
        state.phase = AdmissionPhase::Prepared;
        self.changes.send_replace(state.generation);
        drop(state);
        Ok(ShutdownReservation {
            gate: self,
            committed: false,
        })
    }

    /// Close admission before asynchronous blocker inspection. Force permits existing work,
    /// but never permits another prepared shutdown or new work after this reservation.
    pub fn prepare(&self, force: bool) -> Result<ShutdownReservation<'_>, AdmissionBlocked> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AdmissionBlocked::Unavailable)?;
        if state.phase != AdmissionPhase::Running || (!force && state.active_operations != 0) {
            return Err(AdmissionBlocked::Busy(*state));
        }
        state.advance()?;
        state.phase = AdmissionPhase::Prepared;
        self.changes.send_replace(state.generation);
        drop(state);
        Ok(ShutdownReservation {
            gate: self,
            committed: false,
        })
    }
}

/// Retain until registration is released, not until cancellation is requested.
pub struct OperationAdmission<'a> {
    gate: &'a ShutdownAdmissionGate,
}

impl Drop for OperationAdmission<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active_operations -= 1;
            let _ = state.advance();
            self.gate.changes.send_replace(state.generation);
        }
    }
}

/// Acknowledgment failure drops this reservation and reopens admission.
pub struct ShutdownReservation<'a> {
    gate: &'a ShutdownAdmissionGate,
    committed: bool,
}

impl ShutdownReservation<'_> {
    /// Permanently close admission before signaling the owning shutdown lifecycle.
    pub fn commit(mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.phase = AdmissionPhase::Committed;
            let _ = state.advance();
            self.gate.changes.send_replace(state.generation);
        }
        self.committed = true;
    }
}

impl Drop for ShutdownReservation<'_> {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut state) = self.gate.state.lock()
            && state.phase == AdmissionPhase::Prepared
        {
            state.phase = AdmissionPhase::Running;
            let _ = state.advance();
            self.gate.changes.send_replace(state.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn observed_generation_rejects_intervening_work_even_after_it_settles() {
        let gate = ShutdownAdmissionGate::default();
        let observed = gate.snapshot().unwrap();
        drop(gate.admit().unwrap());
        assert!(gate.prepare_observed(observed.generation).is_err());
        let current = gate.snapshot().unwrap();
        gate.prepare_observed(current.generation).unwrap().commit();
    }

    #[tokio::test]
    async fn notification_retains_registration_and_release_changes_without_polling() {
        let gate = ShutdownAdmissionGate::default();
        let mut changes = gate.subscribe();
        let work = gate.admit().unwrap();
        changes.changed().await.unwrap();
        assert_eq!(gate.snapshot().unwrap().active_operations, 1);
        assert!(gate.prepare(false).is_err());
        drop(work);
        changes.changed().await.unwrap();
        assert_eq!(gate.snapshot().unwrap().active_operations, 0);
    }

    #[test]
    fn new_work_after_status_blocks_preparation() {
        let gate = ShutdownAdmissionGate::default();
        let before = gate.snapshot().unwrap();
        assert_eq!(before.active_operations, 0);
        let work = gate.admit().unwrap();
        assert!(matches!(
            gate.prepare(false),
            Err(AdmissionBlocked::Busy(_))
        ));
        assert!(gate.snapshot().unwrap().generation > before.generation);
        drop(work);
        gate.prepare(false).unwrap().commit();
        assert!(gate.admit().is_err());
    }

    #[test]
    fn failed_acknowledgment_releases_reservation_but_commit_never_does() {
        let gate = ShutdownAdmissionGate::default();
        let prepared = gate.prepare(false).unwrap();
        assert!(gate.admit().is_err());
        assert!(gate.prepare(true).is_err());
        drop(prepared);
        drop(gate.admit().unwrap());
        gate.prepare(false).unwrap().commit();
        assert!(gate.admit().is_err());
        assert!(gate.prepare(true).is_err());
    }

    #[test]
    fn terminal_notification_and_cancellation_are_not_registration_release() {
        let gate = Arc::new(ShutdownAdmissionGate::default());
        let terminal = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let worker_gate = Arc::clone(&gate);
            let terminal_worker = Arc::clone(&terminal);
            let release_worker = Arc::clone(&release);
            scope.spawn(move || {
                let registration = worker_gate.admit().unwrap();
                // The terminal notification is visible while registration is retained.
                terminal_worker.wait();
                release_worker.wait();
                drop(registration);
            });
            terminal.wait();
            assert!(gate.prepare(false).is_err());
            // A cancellation request deliberately does not release the registration.
            assert_eq!(gate.snapshot().unwrap().active_operations, 1);
            assert!(gate.prepare(false).is_err());
            release.wait();
        });
        gate.prepare(false).unwrap().commit();
    }

    #[test]
    fn forced_shutdown_still_excludes_new_admissions() {
        let gate = ShutdownAdmissionGate::default();
        let existing = gate.admit().unwrap();
        let prepared = gate.prepare(true).unwrap();
        assert!(gate.admit().is_err());
        prepared.commit();
        drop(existing);
        assert_eq!(gate.snapshot().unwrap().active_operations, 0);
        assert!(gate.admit().is_err());
    }
}
