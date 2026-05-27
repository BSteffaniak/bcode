use bcode_ipc::RuntimeWorkSnapshot;
use bcode_session_models::{
    RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeWorkItem {
    kind: RuntimeWorkKind,
    label: String,
    status: RuntimeWorkStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeWorkViewState {
    active: BTreeMap<RuntimeWorkId, RuntimeWorkItem>,
}

impl RuntimeWorkViewState {
    pub fn apply_event(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                ..
            } => {
                self.active.insert(
                    work_id.clone(),
                    RuntimeWorkItem {
                        kind: *kind,
                        label: label.clone(),
                        status: RuntimeWorkStatus::Running,
                    },
                );
            }
            SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
                if let Some(item) = self.active.get_mut(work_id) {
                    item.status = RuntimeWorkStatus::Cancelling;
                }
            }
            SessionEventKind::RuntimeWorkFinished { work_id, .. } => {
                self.active.remove(work_id);
            }
            _ => {}
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: &RuntimeWorkSnapshot) {
        self.active.insert(
            snapshot.work_id.clone(),
            RuntimeWorkItem {
                kind: snapshot.kind,
                label: snapshot.label.clone(),
                status: snapshot.status,
            },
        );
    }

    pub fn apply_snapshots(&mut self, snapshots: &[RuntimeWorkSnapshot]) {
        for snapshot in snapshots {
            self.apply_snapshot(snapshot);
        }
    }

    pub fn is_busy(&self) -> bool {
        !self.active.is_empty()
    }

    pub fn is_cancelling(&self) -> bool {
        self.active
            .values()
            .any(|item| item.status == RuntimeWorkStatus::Cancelling)
    }

    pub fn status_label(&self) -> Option<String> {
        let item = self.active.values().next()?;
        let prefix = match item.status {
            RuntimeWorkStatus::Queued => "queued",
            RuntimeWorkStatus::Cancelling => "cancelling",
            RuntimeWorkStatus::Running => match item.kind {
                RuntimeWorkKind::ModelTurn => "running",
                RuntimeWorkKind::Tool => "running tool",
                RuntimeWorkKind::PluginInvocation => "running plugin",
                RuntimeWorkKind::EventDelivery => "delivering event",
            },
            RuntimeWorkStatus::Completed
            | RuntimeWorkStatus::Cancelled
            | RuntimeWorkStatus::Failed
            | RuntimeWorkStatus::TimedOut => return None,
        };
        Some(format!("{prefix}: {}", item.label))
    }
}
