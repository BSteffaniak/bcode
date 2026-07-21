use bcode_ipc::RuntimeWorkSnapshot;
use bcode_session_models::{
    RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind, WorkId,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeWorkItem {
    kind: RuntimeWorkKind,
    label: String,
    status: RuntimeWorkStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeWorkViewState {
    active: BTreeMap<WorkId, RuntimeWorkItem>,
    terminal: BTreeSet<WorkId>,
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
                if self.terminal.contains(work_id) {
                    return;
                }
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
                self.terminal.insert(work_id.clone());
            }
            _ => {}
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: &RuntimeWorkSnapshot) {
        if matches!(
            snapshot.status,
            RuntimeWorkStatus::Completed
                | RuntimeWorkStatus::Cancelled
                | RuntimeWorkStatus::Failed
                | RuntimeWorkStatus::TimedOut
        ) {
            self.active.remove(&snapshot.work_id);
            self.terminal.insert(snapshot.work_id.clone());
            return;
        }
        self.terminal.remove(&snapshot.work_id);
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
        self.terminal.clear();
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

    fn runtime_event(sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind,
        }
    }

    #[test]
    fn aggregate_activity_remains_until_last_sibling_and_late_start_is_ignored() {
        let mut state = RuntimeWorkViewState::default();
        let first = WorkId::new("tool-1");
        let second = WorkId::new("tool-2");
        for (work_id, label) in [(first.clone(), "first"), (second.clone(), "second")] {
            state.apply_event(&runtime_event(
                1,
                SessionEventKind::RuntimeWorkStarted {
                    work_id,
                    kind: RuntimeWorkKind::Tool,
                    label: label.to_owned(),
                    tool_call_id: None,
                    plugin_id: None,
                    service_interface: None,
                    operation: None,
                    parent_work_id: None,
                    started_at_ms: Some(1),
                    cancellable: true,
                },
            ));
        }
        assert_eq!(state.status_label().as_deref(), Some("running 2 tools"));

        state.apply_event(&runtime_event(
            2,
            SessionEventKind::RuntimeWorkFinished {
                work_id: first.clone(),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: Some(2),
                message: None,
            },
        ));
        assert_eq!(
            state.status_label().as_deref(),
            Some("running tool: second")
        );

        state.apply_event(&runtime_event(
            3,
            SessionEventKind::RuntimeWorkStarted {
                work_id: first,
                kind: RuntimeWorkKind::Tool,
                label: "late".to_owned(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(3),
                cancellable: true,
            },
        ));
        assert_eq!(
            state.status_label().as_deref(),
            Some("running tool: second")
        );

        state.apply_event(&runtime_event(
            4,
            SessionEventKind::RuntimeWorkFinished {
                work_id: second.clone(),
                status: RuntimeWorkStatus::Cancelled,
                finished_at_ms: Some(4),
                message: None,
            },
        ));
        assert_eq!(state.status_label(), None);
        state.apply_event(&runtime_event(
            5,
            SessionEventKind::RuntimeWorkStarted {
                work_id: second,
                kind: RuntimeWorkKind::Tool,
                label: "late cancelled".to_owned(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(5),
                cancellable: true,
            },
        ));
        assert_eq!(state.status_label(), None);
    }

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
