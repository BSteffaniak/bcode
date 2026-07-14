use bcode_ipc::RuntimeWorkSnapshot;
use bcode_session_models::{
    RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind, WorkId,
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
    active: BTreeMap<WorkId, RuntimeWorkItem>,
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
            SessionEventKind::RuntimeWorkProgress {
                work_id, message, ..
            } => {
                if let Some(item) = self.active.get_mut(work_id) {
                    item.label = format!("{} — {message}", item.label);
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
        self.active.clear();
        for snapshot in snapshots {
            self.apply_snapshot(snapshot);
        }
    }

    pub fn is_cancelling(&self) -> bool {
        self.active
            .values()
            .any(|item| item.status == RuntimeWorkStatus::Cancelling)
    }

    pub fn status_label(&self) -> Option<String> {
        let running_tools = self
            .active
            .values()
            .filter(|item| {
                item.kind == RuntimeWorkKind::Tool && item.status == RuntimeWorkStatus::Running
            })
            .count();
        if running_tools > 1 {
            return Some(format!("running {running_tools} tools"));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_aggregates_parallel_tools() {
        let mut state = RuntimeWorkViewState::default();
        for index in 0..2 {
            state.apply_snapshot(&RuntimeWorkSnapshot {
                work_id: WorkId::new(format!("tool-{index}")),
                kind: RuntimeWorkKind::Tool,
                label: format!("tool {index}"),
                tool_call_id: Some(format!("call-{index}")),
                status: RuntimeWorkStatus::Running,
                cancellable: true,
            });
        }

        assert_eq!(state.status_label().as_deref(), Some("running 2 tools"));
    }
}
