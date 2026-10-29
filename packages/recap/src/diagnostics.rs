//! Bounded, renderer-neutral operation diagnostics. No source or executable output is retained.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Semantic operation phase; independent of tools and frontends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    /// Decode downloaded timestamped captions.
    Captions,
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
    /// Detailed source inventory and outline analysis.
    Analysis,
    /// Detailed chapter composition.
    Writing,
    /// Source coverage audit.
    Audit,
    /// Source-grounded repair of identified gaps.
    Repair,
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
    /// Bounded aggregate progress, without source identities or editorial content.
    AnalysisProgress {
        /// Semantic stage, not a provider operation or source label.
        phase: Phase,
        /// Number of completed work units.
        completed: u64,
        /// Total planned work units, when known.
        total: Option<u64>,
    },
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
/// Explicitly exportable secret-safe report. Version two adds typed aggregate measurements.
///
/// Consumers must dispatch on version: version-one reports have no measurements field;
/// their absence must not be interpreted as zero usage, no churn, or a successful outcome.
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
    /// Version-two aggregate, independent of bounded event retention.
    pub measurements: Measurements,
}
/// Fixed model-request stages; no arbitrary source or provider labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStage {
    /// Standard composition or refinement.
    Composition,
    /// Detailed source inventory.
    Inventory,
    /// Detailed outline.
    Outline,
    /// Detailed chapter writing.
    Chapter,
    /// Source-based audit.
    Audit,
    /// Targeted repair.
    Repair,
    /// Actual source frame inspection.
    VisualInspection,
}
/// Adapter result, not a claim of semantic validity or provider billing.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// Adapter returned bounded structured output.
    Succeeded,
    /// Adapter failed.
    Failed,
    /// Cancellation was observed.
    Cancelled,
    /// Future dropped without an observed result.
    Interrupted,
}
/// Available normalized token counters. None means unavailable, never zero.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct TokenUsage {
    /// Normalized input tokens.
    pub input_tokens: Option<u64>,
    /// Normalized output tokens.
    pub output_tokens: Option<u64>,
    /// Normalized cached input tokens; not added to input tokens.
    pub cached_input_tokens: Option<u64>,
}
/// Count-only hooks, deliberately separate from the version-one event contract.
#[derive(Clone, Copy, Debug)]
pub enum Measurement {
    /// One adapter request, including requests rejected before invocation.
    Request {
        stage: RequestStage,
        /// Instructions plus serialized input bytes, not token or wire estimates.
        context_bytes: u64,
        /// Original encoded image bytes, before base64 transport encoding.
        image_bytes: u64,
        images: u64,
    },
    /// One invocation of the SDK inference boundary, not provider acceptance.
    Invocation { stage: RequestStage },
    /// One request completed or was dropped.
    Finished {
        stage: RequestStage,
        request_ms: u64,
        /// Time awaiting SDK inference; absent when not invoked.
        inference_ms: Option<u64>,
        outcome: CallOutcome,
        usage: TokenUsage,
    },
    /// Source audit completed with this many findings.
    Audit { findings: u64 },
    /// Repair round attempted this many chapter replacements.
    Repair { chapters: u64 },
}
/// Saturating per-stage counters; bounded by the fixed stage enum.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StageMeasurements {
    /// Requests observed, including pre-inference failures.
    pub requests: u64,
    /// SDK invocations; does not assert provider acceptance or billing.
    pub invocations: u64,
    /// Adapter requests returning bounded structured output.
    pub succeeded: u64,
    /// Adapter failures.
    pub failed: u64,
    /// Observed cancellations.
    pub cancelled: u64,
    /// Requests dropped without an observed result.
    pub interrupted: u64,
    /// Sum of request wall times, not overall elapsed run time.
    pub request_ms: u64,
    /// Sum of available SDK inference wall times.
    pub inference_ms: u64,
    /// Number of requests supplying inference duration.
    pub inference_timing_samples: u64,
    /// Sum of instruction and input UTF-8 bytes, excluding schema/transport overhead.
    pub context_bytes: u64,
    /// Sum of original image bytes.
    pub image_bytes: u64,
    /// Number of images supplied.
    pub images: u64,
    /// Sums of available normalized usage only; missing samples remain unknown.
    pub usage: TokenUsage,
    /// Counts of available input/output/cached token observations respectively.
    pub usage_samples: [u64; 3],
}
/// Typed aggregate carried by report version two. Its own compatibility version is one.
/// Fixed-size counters survive event eviction. No persistence is implicit.
#[derive(Clone, Debug, Serialize)]
pub struct Measurements {
    /// Extension compatibility version, currently one.
    pub version: u32,
    /// Fixed semantic stage keys only.
    pub stages: BTreeMap<RequestStage, StageMeasurements>,
    /// Completed source audits.
    pub audits: u64,
    /// Total audit findings (including repeated findings across rounds).
    pub audit_findings: u64,
    /// Attempted repair rounds.
    pub repair_rounds: u64,
    /// Attempted chapter replacements, not accepted changes.
    pub repair_chapters: u64,
    /// Authoritative observed terminal status, independent of event retention.
    pub terminal: Option<Status>,
    /// Durable commit observed.
    pub committed: bool,
    /// Completed phase wall time in milliseconds, including repeated phase entries.
    pub phase_ms: BTreeMap<Phase, u64>,
}
#[derive(Debug)]
struct State {
    report: Report,
    active: Option<(Phase, u64)>,
}
/// Observer implemented by callers; events contain no raw process/provider content.
pub trait Observer: Send + Sync + std::fmt::Debug {
    /// Observe a semantic event; callers must keep this bounded and nonblocking.
    fn observe(&self, event: Event);
    /// Optional count-only extension. Existing event observers remain compatible.
    fn measure(&self, _measurement: Measurement) {}
}
/// Shared bounded in-memory operation collector; no persistence occurs implicitly.
#[derive(Clone, Debug)]
pub struct Operation {
    started: Instant,
    state: Arc<Mutex<State>>,
}
impl Operation {
    /// Create an operation using a caller-assigned opaque identity.
    #[must_use]
    pub fn new(operation_id: String) -> Self {
        Self {
            started: Instant::now(),
            state: Arc::new(Mutex::new(State {
                report: Report {
                    version: 2,
                    operation_id: if valid_identity(&operation_id) {
                        operation_id
                    } else {
                        "unavailable".into()
                    },
                    events: Vec::new(),
                    truncated: false,
                    inference_started: false,
                    committed: false,
                    measurements: Measurements {
                        version: 1,
                        stages: BTreeMap::new(),
                        audits: 0,
                        audit_findings: 0,
                        repair_rounds: 0,
                        repair_chapters: 0,
                        terminal: None,
                        committed: false,
                        phase_ms: BTreeMap::new(),
                    },
                },
                active: None,
            })),
        }
    }
    /// Snapshot at most 128 events and the complete bounded aggregate.
    #[must_use]
    pub fn report(&self) -> Report {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .report
            .clone()
    }
    /// Snapshot the same bounded aggregate included in normal report export.
    #[must_use]
    pub fn measurements(&self) -> Measurements {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .report
            .measurements
            .clone()
    }
}
impl Observer for Operation {
    fn observe(&self, mut event: Event) {
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.report.measurements.terminal.is_some() {
            return;
        }
        // Only bounded identifiers may enter the retained report.
        match &mut event {
            Event::Terminal { code, .. } => *code = identity(code),
            Event::ModelSelection {
                provider,
                model,
                profile,
            } => {
                *provider = identity(provider);
                *model = identity(model);
                *profile = profile.as_deref().map(identity);
            }
            Event::ToolVersion { version, .. } => *version = identity(version),
            _ => {}
        }
        state.report.inference_started |= matches!(event, Event::InferenceStarted);
        state.report.committed |= matches!(event, Event::Committed);
        state.report.measurements.committed = state.report.committed;
        if matches!(event, Event::Phase { .. } | Event::Terminal { .. })
            && let Some((phase, began)) = state.active.take()
        {
            let duration_ms = elapsed_ms.saturating_sub(began);
            let total = state.report.measurements.phase_ms.entry(phase).or_default();
            *total = total.saturating_add(duration_ms);
            push_event(
                &mut state.report,
                TimedEvent {
                    elapsed_ms,
                    event: Event::PhaseFinished { phase, duration_ms },
                },
            );
        }
        match &event {
            Event::Phase { phase } => {
                state.active = Some((*phase, elapsed_ms));
            }
            Event::Terminal { status, .. } => state.report.measurements.terminal = Some(*status),
            _ => {}
        }
        push_event(&mut state.report, TimedEvent { elapsed_ms, event });
    }
    fn measure(&self, measurement: Measurement) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.report.measurements.terminal.is_some() {
            return;
        }
        let measurements = &mut state.report.measurements;
        match measurement {
            Measurement::Request {
                stage,
                context_bytes,
                image_bytes,
                images,
            } => {
                let counts = measurements.stages.entry(stage).or_default();
                counts.requests = counts.requests.saturating_add(1);
                counts.context_bytes = counts.context_bytes.saturating_add(context_bytes);
                counts.image_bytes = counts.image_bytes.saturating_add(image_bytes);
                counts.images = counts.images.saturating_add(images);
            }
            Measurement::Invocation { stage } => {
                let counts = measurements.stages.entry(stage).or_default();
                counts.invocations = counts.invocations.saturating_add(1);
            }
            Measurement::Finished {
                stage,
                request_ms,
                inference_ms,
                outcome,
                usage,
            } => {
                let counts = measurements.stages.entry(stage).or_default();
                counts.request_ms = counts.request_ms.saturating_add(request_ms);
                if let Some(duration) = inference_ms {
                    counts.inference_ms = counts.inference_ms.saturating_add(duration);
                    counts.inference_timing_samples =
                        counts.inference_timing_samples.saturating_add(1);
                }
                let outcome_count = match outcome {
                    CallOutcome::Succeeded => &mut counts.succeeded,
                    CallOutcome::Failed => &mut counts.failed,
                    CallOutcome::Cancelled => &mut counts.cancelled,
                    CallOutcome::Interrupted => &mut counts.interrupted,
                };
                *outcome_count = outcome_count.saturating_add(1);
                for (index, (total, value)) in [
                    (&mut counts.usage.input_tokens, usage.input_tokens),
                    (&mut counts.usage.output_tokens, usage.output_tokens),
                    (
                        &mut counts.usage.cached_input_tokens,
                        usage.cached_input_tokens,
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    if let Some(value) = value {
                        *total = Some(total.unwrap_or(0).saturating_add(value));
                        counts.usage_samples[index] = counts.usage_samples[index].saturating_add(1);
                    }
                }
            }
            Measurement::Audit { findings } => {
                measurements.audits = measurements.audits.saturating_add(1);
                measurements.audit_findings = measurements.audit_findings.saturating_add(findings);
            }
            Measurement::Repair { chapters } => {
                measurements.repair_rounds = measurements.repair_rounds.saturating_add(1);
                measurements.repair_chapters =
                    measurements.repair_chapters.saturating_add(chapters);
            }
        }
        drop(state);
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
    if valid_identity(value) {
        value.to_owned()
    } else {
        "unavailable".into()
    }
}
fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
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
    fn aggregate_survives_eviction_and_terminal_is_stable() {
        let operation = Operation::new("run".into());
        operation.observe(Event::Phase {
            phase: Phase::Audit,
        });
        operation.observe(Event::Committed);
        for _ in 0..300 {
            operation.observe(Event::AnalysisProgress {
                phase: Phase::Audit,
                completed: 1,
                total: None,
            });
            operation.measure(Measurement::Request {
                stage: RequestStage::Audit,
                context_bytes: 7,
                image_bytes: 11,
                images: 1,
            });
            operation.measure(Measurement::Invocation {
                stage: RequestStage::Audit,
            });
            operation.measure(Measurement::Finished {
                stage: RequestStage::Audit,
                request_ms: 10,
                inference_ms: Some(8),
                outcome: CallOutcome::Succeeded,
                usage: TokenUsage {
                    input_tokens: Some(2),
                    ..TokenUsage::default()
                },
            });
        }
        operation.measure(Measurement::Audit { findings: 3 });
        operation.measure(Measurement::Repair { chapters: 2 });
        operation.observe(Event::Terminal {
            status: Status::ExportFailed,
            code: "export_failed".into(),
        });
        let report = operation.report();
        let counts = operation.measurements();
        assert!(report.truncated);
        assert!(report.committed());
        assert_eq!(counts.terminal, Some(Status::ExportFailed));
        let exported = serde_json::to_value(&report).unwrap();
        assert_eq!(exported["version"], 2);
        assert_eq!(
            exported["measurements"],
            serde_json::to_value(&counts).unwrap()
        );
        assert_eq!(exported["measurements"]["terminal"], "export_failed");
        assert_eq!(exported["measurements"]["committed"], true);
        assert!(counts.committed);
        assert!(counts.phase_ms.contains_key(&Phase::Audit));
        assert!(report.events.iter().any(|event| matches!(
            event.event,
            Event::PhaseFinished {
                phase: Phase::Audit,
                ..
            }
        )));
        let stage = &counts.stages[&RequestStage::Audit];
        assert_eq!(stage.requests, 300);
        assert_eq!(stage.invocations, 300);
        assert_eq!(stage.succeeded, 300);
        assert_eq!(stage.context_bytes, 2100);
        assert_eq!(stage.image_bytes, 3300);
        assert_eq!(stage.images, 300);
        assert_eq!(stage.request_ms, 3000);
        assert_eq!(stage.inference_ms, 2400);
        assert_eq!(stage.inference_timing_samples, 300);
        assert_eq!(stage.usage.input_tokens, Some(600));
        assert_eq!(stage.usage.output_tokens, None);
        assert_eq!(stage.usage_samples, [300, 0, 0]);
        assert_eq!(
            (
                counts.audits,
                counts.audit_findings,
                counts.repair_rounds,
                counts.repair_chapters
            ),
            (1, 3, 1, 2)
        );
        operation.measure(Measurement::Repair { chapters: 99 });
        operation.observe(Event::Terminal {
            status: Status::Succeeded,
            code: "ok".into(),
        });
        assert_eq!(
            serde_json::to_value(counts).unwrap(),
            serde_json::to_value(operation.measurements()).unwrap()
        );
    }

    #[test]
    fn version_two_exports_typed_measurements_and_secret_safe_identifiers() {
        let operation = Operation::new("https://private/path?secret=SECRET".into());
        operation.observe(Event::Terminal {
            status: Status::Failed,
            code: "SECRET\n/private".into(),
        });
        let report = serde_json::to_value(operation.report()).unwrap();
        assert_eq!(report["version"], 2);
        assert_eq!(report.as_object().unwrap().len(), 7);
        assert_eq!(report["measurements"]["version"], 1);
        assert_eq!(report["measurements"]["terminal"], "failed");
        assert_eq!(report["measurements"]["committed"], false);
        assert_eq!(report["operation_id"], "unavailable");
        assert_eq!(report["events"][0]["event"]["code"], "unavailable");
        assert!(!report.to_string().contains("SECRET"));
    }

    #[test]
    fn counters_saturate_and_missing_usage_stays_unknown() {
        let operation = Operation::new("run".into());
        for _ in 0..2 {
            operation.measure(Measurement::Request {
                stage: RequestStage::Repair,
                context_bytes: u64::MAX,
                image_bytes: u64::MAX,
                images: u64::MAX,
            });
            operation.measure(Measurement::Finished {
                stage: RequestStage::Repair,
                request_ms: u64::MAX,
                inference_ms: None,
                outcome: CallOutcome::Failed,
                usage: TokenUsage::default(),
            });
        }
        let stage = &operation.measurements().stages[&RequestStage::Repair];
        assert_eq!(stage.context_bytes, u64::MAX);
        assert_eq!(stage.request_ms, u64::MAX);
        assert_eq!(stage.failed, 2);
        assert_eq!(stage.invocations, 0);
        assert_eq!(stage.inference_timing_samples, 0);
        assert_eq!(stage.usage.input_tokens, None);
    }

    #[test]
    fn detailed_progress_is_count_only_bounded_and_terminal_stable() {
        let operation = Operation::new("test-detailed".into());
        for phase in [Phase::Analysis, Phase::Writing, Phase::Audit, Phase::Repair] {
            operation.observe(Event::Phase { phase });
            operation.observe(Event::AnalysisProgress {
                phase,
                completed: 2,
                total: Some(3),
            });
        }
        let json = serde_json::to_value(operation.report()).unwrap();
        let events = json["events"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"]["kind"] == "analysis_progress")
                .count(),
            4
        );
        operation.observe(Event::Terminal {
            status: Status::Failed,
            code: "coverage_incomplete".into(),
        });
        let terminal = serde_json::to_value(operation.report()).unwrap();
        operation.observe(Event::AnalysisProgress {
            phase: Phase::Repair,
            completed: 3,
            total: Some(3),
        });
        assert_eq!(serde_json::to_value(operation.report()).unwrap(), terminal);
    }
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
