//! Bounded, renderer-neutral operation diagnostics. No source or executable output is retained.
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

/// Semantic operation phase; independent of tools and frontends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Input/configuration validation.
    Validation,
    /// Model resolution, not inference.
    ModelReadiness,
    /// Source metadata request.
    Metadata,
    /// Video/caption acquisition.
    Download,
    /// Local acquisition.
    Acquisition,
    /// Probe source media tracks/duration.
    Probe,
    /// Extract actual stills and silent clips.
    Extraction,
    /// Validate structured editorial output.
    ValidationOutput,
    /// Probe/extract source visuals.
    Media,
    /// Model composition and validation.
    Composition,
    /// Durable revision commit.
    Storage,
    /// HTML generation/write.
    Export,
}
/// Terminal operation outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Operation succeeded.
    Succeeded,
    /// Operation failed.
    Failed,
    /// Operation was cancelled.
    Cancelled,
    /// Revision committed, but export failed.
    ExportFailed,
}
/// Fixed semantic events; arbitrary text is deliberately excluded.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Enter a phase; the next phase or terminal event ends its timing.
    Phase { phase: Phase },
    /// Phase ended; elapsed duration is monotonic and bounded to this operation.
    PhaseFinished { phase: Phase, duration_ms: u64 },
    /// Actual model executor invocation, not readiness.
    InferenceStarted,
    /// Durable document commit succeeded.
    Committed,
    /// Normalized effective model selection.
    ModelSelection {
        provider: String,
        model: String,
        profile: Option<String>,
    },
    /// Bounded normalized tool version, never raw version output.
    ToolVersion { tool: Tool, version: String },
    /// Safe normalized failed acquisition facts.
    AcquisitionFailed {
        phase: Phase,
        exit_code: Option<i32>,
        http_status: Option<u16>,
    },
    /// Terminal operation event.
    Terminal { status: Status, code: String },
}
/// Known media executables; paths are not diagnostics.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    /// `YouTube` downloader.
    YtDlp,
    /// Media extraction.
    Ffmpeg,
    /// Media metadata.
    Ffprobe,
}
/// Monotonic elapsed time and semantic event.
#[derive(Clone, Debug, Serialize)]
pub struct TimedEvent {
    /// Milliseconds since operation start.
    pub elapsed_ms: u64,
    /// Semantic fact.
    pub event: Event,
}
/// Explicitly exportable secret-safe report. Version one rejects reinterpretation by consumers.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Report compatibility version.
    pub version: u32,
    /// Random operation identity, independent of source/model content.
    pub operation_id: String,
    /// Bounded events (at most 128); missing events never imply success.
    pub events: Vec<TimedEvent>,
    /// Whether earlier events were discarded.
    pub truncated: bool,
    /// Model executor invocation observed, retained even when events roll over.
    pub inference_started: bool,
    /// Durable commit observed, retained even when events roll over.
    pub committed: bool,
}
/// Observer implemented by callers; events contain no raw process/provider content.
pub trait Observer: Send + Sync + std::fmt::Debug {
    /// Observe a semantic event; callers must keep this bounded and nonblocking.
    fn observe(&self, event: Event);
}
/// Shared bounded in-memory operation collector; no persistence occurs implicitly.
#[derive(Clone, Debug)]
pub struct Operation {
    started: Instant,
    report: Arc<Mutex<Report>>,
}
impl Operation {
    /// Create an operation using a caller-assigned opaque identity.
    #[must_use]
    pub fn new(operation_id: String) -> Self {
        Self {
            started: Instant::now(),
            report: Arc::new(Mutex::new(Report {
                version: 1,
                operation_id,
                events: Vec::new(),
                truncated: false,
                inference_started: false,
                committed: false,
            })),
        }
    }
    /// Snapshot at most 128 events.
    #[must_use]
    pub fn report(&self) -> Report {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
impl Observer for Operation {
    fn observe(&self, event: Event) {
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut report = self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if report
            .events
            .last()
            .is_some_and(|e| matches!(e.event, Event::Terminal { .. }))
        {
            return;
        }
        report.inference_started |= matches!(event, Event::InferenceStarted);
        report.committed |= matches!(event, Event::Committed);
        if matches!(event, Event::Phase { .. } | Event::Terminal { .. }) {
            let active = report.events.iter().rev().find_map(|e| match e.event {
                Event::Phase { phase } => Some((phase, e.elapsed_ms)),
                _ => None,
            });
            if let Some((phase, began)) = active {
                push_event(
                    &mut report,
                    TimedEvent {
                        elapsed_ms,
                        event: Event::PhaseFinished {
                            phase,
                            duration_ms: elapsed_ms.saturating_sub(began),
                        },
                    },
                );
            }
        }
        push_event(&mut report, TimedEvent { elapsed_ms, event });
        drop(report);
    }
}
fn push_event(report: &mut Report, event: TimedEvent) {
    if report.events.len() == 128 {
        report.events.remove(0);
        report.truncated = true;
    }
    report.events.push(event);
}

/// Keep configuration-controlled identifiers bounded and terminal-safe. Never use for credentials.
#[must_use]
pub fn identity(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        value.to_owned()
    } else {
        "unavailable".into()
    }
}
impl Report {
    /// Last observed phase; absent means no phase was observed.
    // Option already carries must-use semantics; repository policy forbids redundant attributes.
    #[allow(clippy::must_use_candidate)]
    pub fn phase(&self) -> Option<Phase> {
        for event in self.events.iter().rev() {
            if let Event::Phase { phase } = event.event {
                return Some(phase);
            }
        }
        None
    }
    /// Whether the model executor was invoked. Does not claim provider acceptance/billing.
    #[must_use]
    pub const fn inference_started(&self) -> bool {
        self.inference_started
    }
    /// Whether a durable commit was observed.
    #[must_use]
    pub const fn committed(&self) -> bool {
        self.committed
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_is_stable_and_phase_durations_close() {
        let op = Operation::new("test".into());
        op.observe(Event::Phase {
            phase: Phase::Download,
        });
        op.observe(Event::InferenceStarted);
        op.observe(Event::Committed);
        op.observe(Event::Terminal {
            status: Status::Succeeded,
            code: "ok".into(),
        });
        let settled = serde_json::to_vec(&op.report()).unwrap();
        op.observe(Event::Phase {
            phase: Phase::Media,
        });
        assert_eq!(settled, serde_json::to_vec(&op.report()).unwrap());
        assert!(op.report().inference_started());
        assert!(op.report().committed());
        assert!(op.report().events.iter().any(|e| matches!(
            e.event,
            Event::PhaseFinished {
                phase: Phase::Download,
                ..
            }
        )));
        assert_eq!(
            identity("secret=https://example?token=PRIVATE\u{1b}[2J"),
            "unavailable"
        );
    }
    #[test]
    fn bounded_events_preserve_terminal_outcome() {
        let op = Operation::new("test".into());
        for _ in 0..200 {
            op.observe(Event::Phase {
                phase: Phase::Media,
            });
        }
        op.observe(Event::Terminal {
            status: Status::Failed,
            code: "process_timeout".into(),
        });
        let report = op.report();
        assert_eq!(report.events.len(), 128);
        assert!(report.truncated);
        assert!(!report.inference_started());
        assert!(!report.committed());
        assert!(matches!(
            report.events.last().unwrap().event,
            Event::Terminal {
                status: Status::Failed,
                ..
            }
        ));
    }
}
