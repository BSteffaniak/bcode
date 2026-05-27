use bcode_session_models::{RuntimeWorkId, RuntimeWorkStatus, SessionEvent, SessionEventKind};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeWorkViewState {
    active: BTreeMap<RuntimeWorkId, RuntimeWorkStatus>,
}

impl RuntimeWorkViewState {
    pub fn apply_event(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::RuntimeWorkStarted { work_id, .. } => {
                self.active
                    .insert(work_id.clone(), RuntimeWorkStatus::Running);
            }
            SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
                self.active
                    .insert(work_id.clone(), RuntimeWorkStatus::Cancelling);
            }
            SessionEventKind::RuntimeWorkFinished { work_id, .. } => {
                self.active.remove(work_id);
            }
            _ => {}
        }
    }

    pub fn is_busy(&self) -> bool {
        !self.active.is_empty()
    }

    pub fn is_cancelling(&self) -> bool {
        self.active
            .values()
            .any(|status| *status == RuntimeWorkStatus::Cancelling)
    }
}
