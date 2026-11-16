//! Retained native lifecycle transaction bookkeeping, independent of observers.
use bmux_plugin_sdk::abi::NativeOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Created,
    Activating,
    Active,
    Closing,
    CleanupRequired,
    Inactive,
    Destroyed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StepKind {
    Activate,
    Contributions,
    Deactivate,
}
pub(super) struct Step {
    pub id: u64,
    pub kind: StepKind,
    pub outcome: Option<NativeOutcome>,
}
pub(super) struct Ledger {
    pub phase: Phase,
    pub pending: Option<Step>,
    pub failed_lifecycle: Vec<u64>,
    pub failures: Vec<String>,
    pub cleanup_completed: bool,
    pub activation_started: bool,
}
impl Ledger {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Created,
            pending: None,
            failed_lifecycle: Vec::new(),
            failures: Vec::new(),
            cleanup_completed: false,
            activation_started: false,
        }
    }
    pub fn started(&mut self, id: u64, kind: StepKind) -> Result<(), &'static str> {
        if self.pending.is_some() {
            return Err("native lifecycle operation already retained");
        }
        if kind == StepKind::Activate {
            self.activation_started = true;
        }
        self.pending = Some(Step {
            id,
            kind,
            outcome: None,
        });
        Ok(())
    }
    pub fn observe(&mut self, outcome: NativeOutcome) -> Result<(), &'static str> {
        let step = self
            .pending
            .as_mut()
            .ok_or("native lifecycle operation missing")?;
        if step.outcome.is_some() {
            return Err("native lifecycle outcome already observed");
        }
        step.outcome = Some(outcome);
        Ok(())
    }
    pub fn failed(&mut self, stage: &str, details: &str) {
        self.phase = Phase::CleanupRequired;
        if self.failures.len() == 128 {
            self.failures.remove(0);
        }
        self.failures.push(format!(
            "{stage}: {}",
            details.chars().take(4096).collect::<String>()
        ));
    }
    pub fn retain_failed_lifecycle(&mut self) -> Result<(), &'static str> {
        let step = self
            .pending
            .as_ref()
            .ok_or("native lifecycle operation missing")?;
        if step.kind == StepKind::Contributions
            || !matches!(step.outcome, Some(NativeOutcome::Error { .. }))
        {
            return Err("only failed terminal lifecycle outcomes use repaired cleanup");
        }
        self.failed_lifecycle.push(step.id);
        self.pending = None;
        Ok(())
    }
    pub fn acknowledged(&mut self) -> Result<StepKind, &'static str> {
        let step = self
            .pending
            .as_ref()
            .ok_or("native lifecycle operation missing")?;
        if step.outcome.is_none() {
            return Err("cannot acknowledge unobserved native outcome");
        }
        let kind = step.kind;
        self.pending = None;
        if kind == StepKind::Deactivate {
            self.failed_lifecycle.clear();
            self.phase = Phase::Inactive;
            self.cleanup_completed = true;
        }
        Ok(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_cleanup_is_retained_until_successful_repaired_acknowledgement() {
        let mut ledger = Ledger::new();
        ledger.started(1, StepKind::Activate).expect("activation");
        ledger
            .observe(NativeOutcome::Error {
                code: 9,
                message: "partial activation".to_string(),
            })
            .expect("outcome");
        ledger.failed("activation", "partial activation");
        ledger.retain_failed_lifecycle().expect("retention");
        assert_eq!(ledger.failed_lifecycle, vec![1]);
        ledger.started(2, StepKind::Deactivate).expect("cleanup");
        ledger
            .observe(NativeOutcome::Error {
                code: 10,
                message: "cleanup failed".to_string(),
            })
            .expect("failed cleanup");
        ledger.retain_failed_lifecycle().expect("retain cleanup");
        assert_eq!(ledger.failed_lifecycle, vec![1, 2]);
        ledger
            .started(3, StepKind::Deactivate)
            .expect("explicit retry");
        ledger
            .observe(NativeOutcome::Unit)
            .expect("repaired cleanup");
        assert_eq!(
            ledger.failed_lifecycle,
            vec![1, 2],
            "observing success alone never releases outcomes"
        );
        ledger
            .acknowledged()
            .expect("checked runtime acknowledgement succeeded");
        assert_eq!(ledger.failed_lifecycle, Vec::<u64>::new());
        assert!(
            !ledger.failures.is_empty(),
            "repair does not erase diagnostic history"
        );
    }

    #[test]
    fn acknowledgement_retry_reuses_retained_terminal_outcome_without_reexecution() {
        let mut ledger = Ledger::new();
        ledger.started(7, StepKind::Deactivate).expect("cleanup");
        assert!(ledger.acknowledged().is_err());
        ledger.observe(NativeOutcome::Unit).expect("terminal");
        assert!(
            ledger.started(8, StepKind::Deactivate).is_err(),
            "an ack failure cannot start another cleanup hook"
        );
        assert_eq!(ledger.pending.as_ref().expect("retained").id, 7);
        assert!(matches!(
            ledger.pending.as_ref().expect("retained").outcome,
            Some(NativeOutcome::Unit)
        ));
        ledger.acknowledged().expect("ack retry settled");
        assert_eq!(ledger.phase, Phase::Inactive);
    }
}
