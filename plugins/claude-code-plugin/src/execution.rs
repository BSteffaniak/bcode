//! Bounded native execution evidence, independent of legacy session settlement.
#[path = "execution_hydration.rs"]
mod hydration;

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
use crate::RetainedAttribution;
use bcode_claude_code::{AttributionStage, Event, NativeMessage, SubmissionAttribution};
use bcode_harness_models::{
    HarnessAttemptIdentity, HarnessContractError, HarnessExecutionEvent as Execution,
    HarnessNotificationState as Notification, HarnessTerminalDisposition as Disposition,
    HarnessTrackingCoverage as Coverage, HarnessWorkKind as Kind, HarnessWorkState as State,
    MAX_HARNESS_EXECUTION_ITEMS, MAX_HARNESS_TEXT_BYTES,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, HarnessContractError>;
// Native task/run identifiers are independent opaque namespaces. Delimiter
// concatenation aliases e.g. ("a:b", "c") with ("a", "b:c").
type WorkKey = (String, Option<String>);

#[derive(Clone)]
struct Work {
    id: String,
    task: String,
    tool: Option<String>,
    run: Option<String>,
    origin: Option<HarnessAttemptIdentity>,
    parent: Option<String>,
    kind: Kind,
    state: State,
    notification: Notification,
}
impl Work {
    fn event(&self) -> Execution {
        Execution::BackgroundWork {
            work_id: self.id.clone(),
            native_task_id: Some(self.task.clone()),
            origin: self.origin.clone(),
            parent_work_id: self.parent.clone(),
            work_kind: self.kind,
            label: "Native background work".into(),
            state: self.state,
            notification: self.notification,
        }
    }
}
#[derive(Clone)]
struct Followup {
    id: String,
    origin: Option<HarnessAttemptIdentity>,
    work: Option<String>,
    state: State,
    result_frame: Option<String>,
    output: Option<String>,
}
impl Followup {
    fn event(&self) -> Execution {
        Execution::Continuation {
            continuation_id: self.id.clone(),
            origin: self.origin.clone(),
            source_work_id: self.work.clone(),
            native_turn_id: None,
            state: self.state,
            output: self.output.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeSupport {
    Unconfigured,
    Unsupported,
    Supported,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Initialization {
    AwaitingNativeEvidence,
    Observed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheDebt {
    None,
    CanonicalOutstanding,
}

#[derive(Clone)]
pub struct Tracker {
    attempts: BTreeSet<HarnessAttemptIdentity>,
    frames: BTreeMap<String, Vec<HarnessAttemptIdentity>>,
    terminals: BTreeMap<HarnessAttemptIdentity, Execution>,
    tools: BTreeMap<String, Option<HarnessAttemptIdentity>>,
    pending_background_tools: BTreeSet<String>,
    work: BTreeMap<WorkKey, Work>,
    followups: BTreeMap<String, Followup>,
    current: Option<HarnessAttemptIdentity>,
    active_followup: Option<String>,
    seen_results: BTreeMap<String, Value>,
    last_coverage: Vec<Execution>,
    cache_debt: CacheDebt,
    retirement_ready: bool,
    hydrated_origins: BTreeSet<HarnessAttemptIdentity>,
    acknowledged_terminals: BTreeSet<HarnessAttemptIdentity>,
    pending_publication: BTreeMap<String, String>,
    counter: u64,
    supported: NativeSupport,
    initialized: Initialization,
    background_known: bool,
    continuations_known: bool,
}
impl Tracker {
    pub(crate) const fn new() -> Self {
        Self {
            attempts: BTreeSet::new(),
            frames: BTreeMap::new(),
            terminals: BTreeMap::new(),
            tools: BTreeMap::new(),
            pending_background_tools: BTreeSet::new(),
            work: BTreeMap::new(),
            followups: BTreeMap::new(),
            current: None,
            active_followup: None,
            seen_results: BTreeMap::new(),
            last_coverage: Vec::new(),
            cache_debt: CacheDebt::None,
            retirement_ready: false,
            hydrated_origins: BTreeSet::new(),
            acknowledged_terminals: BTreeSet::new(),
            pending_publication: BTreeMap::new(),
            counter: 0,
            supported: NativeSupport::Unconfigured,
            initialized: Initialization::AwaitingNativeEvidence,
            background_known: false,
            continuations_known: false,
        }
    }
    /// Exact-version producer attestation and a fresh conversation are both required.
    pub(crate) const fn configure(&mut self, supported: bool, fresh: bool) {
        self.supported = if supported {
            NativeSupport::Supported
        } else {
            NativeSupport::Unsupported
        };
        self.background_known = supported && fresh;
        self.continuations_known = supported && fresh;
    }
    fn next_id(&mut self, prefix: &str) -> Result<String> {
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(HarnessContractError::BoundsExceeded)?;
        Ok(format!("claude-{prefix}-{}", self.counter))
    }
    pub(crate) fn published(&mut self, event: &Execution, observation: &str) {
        let key = match event {
            Execution::SubmissionTerminal { attempt, .. } => {
                format!("attempt:{}", attempt.attempt_id)
            }
            Execution::BackgroundWork { work_id, .. } => format!("work:{work_id}"),
            Execution::Continuation {
                continuation_id, ..
            } => format!("continuation:{continuation_id}"),
            Execution::Coverage { attempt, .. } => format!("coverage:{}", attempt.attempt_id),
        };
        self.pending_publication.insert(key, observation.to_owned());
    }
    #[cfg(test)]
    pub fn has_cached_work(&self) -> bool {
        !self.work.is_empty() || !self.followups.is_empty()
    }
    pub fn has_unacknowledged(&self) -> bool {
        !self.pending_publication.is_empty()
    }
    pub(crate) fn acknowledge(&mut self, observation: &str) {
        self.pending_publication
            .retain(|_, pending| pending != observation);
    }
    pub fn retire_canonical_attempts(&mut self) -> Vec<HarnessAttemptIdentity> {
        // This releases storage only. Child obligations/failures remain in the
        // canonical root, and late native references require hydration before use.
        let retired: Vec<_> = self
            .acknowledged_terminals
            .iter()
            .filter(|attempt| {
                !self
                    .pending_publication
                    .contains_key(&format!("attempt:{}", attempt.attempt_id))
                    && !self
                        .pending_publication
                        .contains_key(&format!("coverage:{}", attempt.attempt_id))
            })
            .cloned()
            .collect();
        for attempt in &retired {
            self.attempts.remove(attempt);
            self.terminals.remove(attempt);
            self.frames
                .retain(|_, attempts| !attempts.contains(attempt));
            self.acknowledged_terminals.remove(attempt);
            self.tools
                .retain(|_, origin| origin.as_ref() != Some(attempt));
        }
        if !retired.is_empty() {
            self.seen_results.clear();
        }
        self.last_coverage.retain(|event| !matches!(event, Execution::Coverage { attempt, .. } if retired.contains(attempt)));
        retired
    }
    pub(crate) fn retire_completed(&mut self) -> Vec<HarnessAttemptIdentity> {
        // Only an explicit acknowledged prefix can retire evidence. Admission,
        // result delivery, or an empty inventory is never a retirement boundary.
        if !self.retirement_ready
            || self.cache_debt == CacheDebt::CanonicalOutstanding
            || !self.pending_publication.is_empty()
            || !self.background_known
            || !self.continuations_known
            || self.work.values().any(|work| work.origin.is_none())
            || self
                .followups
                .values()
                .any(|followup| followup.origin.is_none())
        {
            return Vec::new();
        }
        let retired: BTreeSet<_> = self
            .attempts
            .iter()
            .filter(|attempt| {
                self.terminals.contains_key(*attempt)
                    && self.pending_background_tools.is_empty()
                    && self
                        .work
                        .values()
                        .filter(|work| work.origin.as_ref() == Some(*attempt))
                        .all(|work| {
                            work.state.is_terminal() && work.notification == Notification::Observed
                        })
                    && self
                        .followups
                        .values()
                        .filter(|followup| followup.origin.as_ref() == Some(*attempt))
                        .all(|followup| followup.state.is_terminal())
            })
            .cloned()
            .collect();
        self.attempts.retain(|attempt| !retired.contains(attempt));
        self.terminals
            .retain(|attempt, _| !retired.contains(attempt));
        self.frames
            .retain(|_, attempts| !attempts.iter().all(|attempt| retired.contains(attempt)));
        self.tools.retain(|_, origin| {
            origin
                .as_ref()
                .is_none_or(|origin| !retired.contains(origin))
        });
        self.work.retain(|_, work| {
            work.origin
                .as_ref()
                .is_none_or(|origin| !retired.contains(origin))
        });
        self.followups.retain(|_, followup| {
            followup
                .origin
                .as_ref()
                .is_none_or(|origin| !retired.contains(origin))
        });
        // Canonical host evidence remains authoritative. Eviction is not nonexecution
        // or replay authority; an old frame no longer provable is uncertainty.
        if !retired.is_empty() {
            self.seen_results.clear();
        }
        self.last_coverage.retain(|event| !matches!(event, Execution::Coverage { attempt, .. } if retired.contains(attempt)));
        retired.into_iter().collect()
    }
    pub(crate) fn check_retain(&self) -> Result<()> {
        if self.attempts.len() >= MAX_HARNESS_EXECUTION_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        Ok(())
    }
    pub(crate) fn retain(&mut self, attempt: HarnessAttemptIdentity) -> Result<()> {
        self.check_retain()?;
        self.attempts.insert(attempt);
        Ok(())
    }
    pub(crate) fn attribute(
        &mut self,
        evidence: &SubmissionAttribution,
        retained: &[RetainedAttribution],
    ) -> Result<()> {
        if evidence.resumed {
            self.lose_coverage();
            return Ok(());
        }
        let attempts: Vec<_> = retained
            .iter()
            .filter(|entry| entry.evidence.native_frame_uuid == evidence.native_frame_uuid)
            .map(|entry| HarnessAttemptIdentity {
                submission_id: entry.submission_id.clone(),
                attempt_id: entry.attempt_id.clone(),
            })
            .collect();
        if attempts.len() != evidence.consumed.len()
            || attempts
                .iter()
                .any(|attempt| !self.attempts.contains(attempt))
        {
            return Err(HarnessContractError::InvalidShape);
        }
        if self.frames.len() >= MAX_HARNESS_EXECUTION_ITEMS
            && !self.frames.contains_key(&evidence.native_frame_uuid)
        {
            // Only result-frame attribution is needed after the first reply has
            // established current origin. Drop consumed reply stamps, not terminals.
            self.frames.retain(|_, attempts| {
                !attempts
                    .iter()
                    .all(|attempt| self.terminals.contains_key(attempt))
            });
            if self.frames.len() >= MAX_HARNESS_EXECUTION_ITEMS {
                return Err(HarnessContractError::BoundsExceeded);
            }
        }
        if matches!(evidence.stage, AttributionStage::Result { .. }) {
            self.frames
                .insert(evidence.native_frame_uuid.clone(), attempts.clone());
        }
        if evidence.stage == AttributionStage::Reply {
            // Merged replies cannot establish exclusive tool/work origin.
            self.current = (attempts.len() == 1).then(|| attempts[0].clone());
        }
        Ok(())
    }
    pub(crate) const fn lose_coverage(&mut self) {
        self.retirement_ready = false;
        self.background_known = false;
        self.continuations_known = false;
    }
    fn incomplete_execution(&mut self) -> Vec<Execution> {
        self.lose_coverage();
        let mut events = Vec::new();
        for work in self.work.values_mut() {
            if !work.state.is_terminal() {
                work.state = State::Unknown;
            }
            // Completion notification uncertainty never alters a terminal work outcome.
            if work.notification == Notification::Pending {
                work.notification = Notification::Unknown;
            }
            events.push(work.event());
        }
        for followup in self.followups.values_mut() {
            if !followup.state.is_terminal() {
                followup.state = State::Unknown;
            }
            events.push(followup.event());
        }
        events.extend(self.coverage());
        events
    }
    pub(crate) fn disconnected(&mut self) -> Vec<Execution> {
        self.incomplete_execution()
    }
    pub(crate) fn coverage(&self) -> Vec<Execution> {
        self.attempts
            .iter()
            .map(|attempt| Execution::Coverage {
                attempt: attempt.clone(),
                background: if self.supported == NativeSupport::Supported
                    && self.initialized == Initialization::Observed
                    && self.background_known
                    && self.pending_background_tools.is_empty()
                    && self.work.values().all(|work| work.kind != Kind::Other)
                {
                    Coverage::Complete
                } else {
                    Coverage::Unknown
                },
                continuations: if self.supported == NativeSupport::Supported
                    && self.initialized == Initialization::Observed
                    && self.continuations_known
                {
                    Coverage::Complete
                } else {
                    Coverage::Unknown
                },
            })
            .collect()
    }
    pub(crate) fn event(&mut self, event: &Event) -> Result<Vec<Execution>> {
        if self.attempts.is_empty() && self.supported == NativeSupport::Unconfigured {
            return Ok(Vec::new());
        }
        let value = match event {
            Event::Message(
                NativeMessage::System(value)
                | NativeMessage::Assistant(value)
                | NativeMessage::User(value)
                | NativeMessage::Result(value)
                | NativeMessage::Stream(value),
            ) => value,
            Event::Settlement(bcode_claude_code::Settlement::Uncertain) => {
                return Ok(self.incomplete_execution());
            }
            _ => return Ok(Vec::new()),
        };
        if value.get("type").and_then(Value::as_str) != Some("system")
            && value
                .get("parent_tool_use_id")
                .is_some_and(|id| !id.is_null())
        {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        if value.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification") {
            // Externally injected notifications and session-task wake deliveries are
            // not interchangeable. Native metadata, never text/quiet time, controls it.
            self.current = None;
        }
        if value.get("resume_reason").is_some()
            || value.get("reason").and_then(Value::as_str) == Some("worker_restart")
        {
            self.lose_coverage();
        }
        events.extend(match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.assistant(value)?,
            Some("result") => self.result(value)?,
            Some("system") => self.system(value)?,
            Some("stream_event") => {
                let events = self.followup_start(value)?;
                // Tool input is incomplete until the full assistant envelope. Never
                // claim complete background tracking across a streamed tool start.
                if value
                    .pointer("/event/content_block/type")
                    .and_then(Value::as_str)
                    == Some("tool_use")
                {
                    let block = value
                        .pointer("/event/content_block")
                        .ok_or(HarnessContractError::InvalidShape)?;
                    let tool = field(block, "id")?;
                    if self.pending_background_tools.len() >= MAX_HARNESS_EXECUTION_ITEMS {
                        return Err(HarnessContractError::BoundsExceeded);
                    }
                    self.pending_background_tools.insert(tool.to_owned());
                }
                events
            }
            Some("user") => {
                let mut events = self.followup_start(value)?;
                events.extend(self.tool_result(value)?);
                events
            }
            _ => Vec::new(),
        });
        let coverage = self.coverage();
        if coverage != self.last_coverage {
            self.last_coverage.clone_from(&coverage);
            events.extend(coverage);
        }
        Ok(events)
    }
    fn assistant(&mut self, value: &Value) -> Result<Vec<Execution>> {
        if let Some(uuid) = value.get("user_message_uuid")
            && !self
                .attempts
                .iter()
                .any(|attempt| uuid.as_str() == Some(attempt.attempt_id.as_str()))
        {
            // Defense in depth for direct/staged adapter input: explicit stale
            // provenance is not unattributed activity belonging to current.
            self.lose_coverage();
            return Ok(self.coverage());
        }
        let mut events = self.followup_start(value)?;
        if self.active_followup.is_none()
            && value.get("user_message_uuid").is_none()
            && self.current.is_none()
            && !self.terminals.is_empty()
        {
            // A wake without native provenance cannot be attached by timing, even
            // if only one notification is pending.
            self.continuations_known = false;
        }
        if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let id = field(block, "id")?;
                if self.tools.len() >= MAX_HARNESS_EXECUTION_ITEMS && !self.tools.contains_key(id) {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                if block
                    .pointer("/input/run_in_background")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    self.pending_background_tools.insert(id.to_owned());
                } else {
                    self.pending_background_tools.remove(id);
                }
                let origin = self
                    .active_followup
                    .as_ref()
                    .and_then(|id| self.followups.get(id))
                    .and_then(|f| f.origin.clone())
                    .or_else(|| self.current.clone());
                if let Some(previous) = self.tools.get(id) {
                    if previous != &origin {
                        return Err(HarnessContractError::InvalidShape);
                    }
                } else {
                    self.tools.insert(id.into(), origin);
                }
            }
        }
        // A running follow-up is independent of any already-terminal foreground.
        if value.get("resume_reason").is_some() {
            self.lose_coverage();
            events.extend(self.coverage());
        }
        Ok(events)
    }
    fn result(&mut self, value: &Value) -> Result<Vec<Execution>> {
        let frame = field(value, "uuid")?;
        let stamp = serde_json::json!({
            "subtype": value.get("subtype"), "is_error": value.get("is_error"),
            "result": output(value)?, "origin_kind": value.pointer("/origin/kind"),
            "origin_run": value.pointer("/origin/runId"),
        });
        if let Some(previous) = self.seen_results.get(frame) {
            return if previous == &stamp {
                Ok(Vec::new())
            } else {
                Err(HarnessContractError::InvalidShape)
            };
        }
        if self.seen_results.len() >= MAX_HARNESS_EXECUTION_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        self.retirement_ready = false;
        let mut events = if value.pointer("/origin/runId").is_some()
            && value.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
            && value.pointer("/origin/producer").and_then(Value::as_str) == Some("session-task")
        {
            self.followup_start(value)?
        } else {
            Vec::new()
        };
        let output = output(value)?;
        let disposition = disposition(value)?;
        if self.frames.contains_key(frame) && value.get("resume_reason").is_some() {
            self.lose_coverage();
            return Ok(self.coverage());
        }
        if self.frames.contains_key(frame) {
            let attempts = self
                .frames
                .get(frame)
                .ok_or(HarnessContractError::InvalidShape)?;
            for attempt in attempts {
                let terminal = Execution::SubmissionTerminal {
                    attempt: attempt.clone(),
                    native_turn_id: None,
                    native_frame_id: Some(frame.into()),
                    disposition,
                    output: if attempts.len() == 1 {
                        output.clone()
                    } else {
                        None
                    },
                };
                if let Some(previous) = self.terminals.get(attempt) {
                    // Another result/worker run must not overwrite an immutable terminal.
                    if previous != &terminal {
                        self.continuations_known = false;
                    }
                } else {
                    self.terminals.insert(attempt.clone(), terminal.clone());
                    events.push(terminal);
                }
            }
            self.current = None;
        }
        let ending_followup = self.matching_followup_result(value);
        if let Some(id) = ending_followup {
            if self.active_followup.as_ref() == Some(&id) {
                self.active_followup = None;
            }
            let followup = self
                .followups
                .get_mut(&id)
                .ok_or(HarnessContractError::InvalidShape)?;
            if !followup.state.is_terminal() {
                followup.state = if disposition == Disposition::Completed {
                    State::Completed
                } else {
                    State::Failed
                };
                followup.result_frame = Some(frame.to_owned());
                followup.output = output;
                events.push(followup.event());
            }
        } else if !self.frames.contains_key(frame) {
            self.continuations_known = false;
        }
        self.seen_results.insert(frame.into(), stamp);
        Ok(events)
    }
    fn matching_followup_result(&self, value: &Value) -> Option<String> {
        // A result must explicitly name the same causal native task run. Active
        // pointer/order alone cannot associate delayed foreground or wake results.
        if value.pointer("/origin/kind").and_then(Value::as_str) != Some("task-notification")
            || value.pointer("/origin/producer").and_then(Value::as_str) != Some("session-task")
        {
            return None;
        }
        let run = value.pointer("/origin/runId").and_then(Value::as_str)?;
        let mut matching = self
            .work
            .values()
            .filter(|work| work.run.as_deref() == Some(run));
        let work = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let id = format!("claude-followup-{}", work.id);
        self.followups
            .get(&id)
            .filter(|followup| followup.work.as_ref() == Some(&work.id))?;
        Some(id)
    }
    fn system(&mut self, value: &Value) -> Result<Vec<Execution>> {
        match value.get("subtype").and_then(Value::as_str) {
            Some("session_state_changed") => {
                // Idle is solely an explicit cache-retirement boundary here, never
                // a terminal, a notification acknowledgement, or work settlement.
                self.retirement_ready = value.get("state").and_then(Value::as_str) == Some("idle");
                Ok(Vec::new())
            }
            Some("init") => {
                self.initialized = Initialization::Observed;
                Ok(Vec::new())
            }
            Some("background_tasks_changed") => self.snapshot(value),
            Some("task_started" | "task_updated" | "task_notification") => self.task(value),
            _ => Ok(Vec::new()),
        }
    }
    fn tool_result(&mut self, value: &Value) -> Result<Vec<Execution>> {
        let Some(task) = value
            .pointer("/tool_use_result/backgroundTaskId")
            .or_else(|| value.pointer("/tool_use_result/background_task_id"))
            .and_then(Value::as_str)
        else {
            return Ok(Vec::new());
        };
        let mut tool_id = None;
        if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let id = field(block, "tool_use_id")?;
                    if tool_id.is_some() {
                        self.background_known = false;
                        return Ok(Vec::new());
                    }
                    tool_id = Some(id);
                }
            }
        }
        let native =
            serde_json::json!({"task_id":task,"task_type":"local_bash","tool_use_id":tool_id});
        if let Some(tool) = tool_id {
            self.pending_background_tools.remove(tool);
        }
        let key = self.ensure_work(&native)?;
        if let Some(origin) = tool_id
            .and_then(|tool| self.tools.get(tool))
            .cloned()
            .flatten()
        {
            let work = self
                .work
                .get_mut(&key)
                .ok_or(HarnessContractError::InvalidShape)?;
            if work.origin.as_ref().is_some_and(|old| old != &origin) {
                return Err(HarnessContractError::InvalidShape);
            }
            work.origin = Some(origin);
        }
        Ok(vec![self.work[&key].event()])
    }
    fn work_key(value: &Value) -> Result<WorkKey> {
        let task = field(value, "task_id")?;
        let run = value
            .get("run_id")
            .map(|_| field(value, "run_id"))
            .transpose()?;
        Ok((task.to_owned(), run.map(str::to_owned)))
    }
    fn enrich_work(&mut self, key: &WorkKey, value: &Value) -> Result<()> {
        let kind = value
            .get("task_type")
            .and_then(Value::as_str)
            .map(|kind| match kind {
                "local_bash" => Kind::Command,
                "local_agent" | "local_workflow" => Kind::Subagent,
                _ => Kind::Other,
            });
        let origin = value
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|tool| self.tools.get(tool))
            .cloned()
            .flatten();
        if origin
            .as_ref()
            .is_some_and(|attempt| !self.attempts.contains(attempt))
        {
            return Err(HarnessContractError::InvalidShape);
        }
        let work = self
            .work
            .get_mut(key)
            .ok_or(HarnessContractError::InvalidShape)?;
        if kind.is_some_and(|kind| {
            work.kind != Kind::Other && kind != Kind::Other && kind != work.kind
        }) || origin
            .as_ref()
            .is_some_and(|origin| work.origin.as_ref().is_some_and(|old| old != origin))
        {
            return Err(HarnessContractError::InvalidShape);
        }
        if let Some(tool) = value.get("tool_use_id").and_then(Value::as_str) {
            if work.tool.as_deref().is_some_and(|old| old != tool) {
                return Err(HarnessContractError::InvalidShape);
            }
            work.tool = Some(tool.to_owned());
        }
        // Monotonic metadata enrichment only; no known association or class changes.
        if work.kind == Kind::Other
            && let Some(kind) = kind
        {
            work.kind = kind;
        }
        if work.origin.is_none() {
            work.origin = origin;
        }
        Ok(())
    }
    fn ensure_work(&mut self, value: &Value) -> Result<WorkKey> {
        let key = Self::work_key(value)?;
        if self.work.contains_key(&key) {
            self.enrich_work(&key, value)?;
            return Ok(key);
        }
        let task = field(value, "task_id")?;
        let run = value.get("run_id").and_then(Value::as_str);
        let candidates: Vec<_> = self
            .work
            .iter()
            .filter(|(_, work)| {
                work.task == task
                    && (run.is_none() || (work.run.is_none() && !work.state.is_terminal()))
            })
            .map(|(key, _)| key.clone())
            .collect();
        if candidates.len() > 1 {
            // A task-only update cannot select among several native runs.
            return Err(HarnessContractError::InvalidShape);
        }
        if candidates.len() == 1 {
            let old = &candidates[0];
            if run.is_none() {
                return Ok(old.clone());
            }
            let mut work = self
                .work
                .remove(old)
                .ok_or(HarnessContractError::InvalidShape)?;
            work.run = run.map(str::to_owned);
            self.work.insert(key.clone(), work);
            return Ok(key);
        }
        if self.work.len() >= MAX_HARNESS_EXECUTION_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        let kind = match value.get("task_type").and_then(Value::as_str) {
            Some("local_bash") => Kind::Command,
            Some("local_agent" | "local_workflow") => Kind::Subagent,
            Some(_) | None => Kind::Other,
        };
        let origin = value
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|tool| self.tools.get(tool))
            .cloned()
            .flatten();
        let parent: Option<String> = value
            .get("parent_task_id")
            .and_then(Value::as_str)
            .and_then(|task| {
                let matches: Vec<_> = self
                    .work
                    .values()
                    .filter(|work| work.task == task)
                    .collect();
                (matches.len() == 1).then(|| matches[0].id.clone())
            });
        let origin = origin.or_else(|| {
            parent
                .as_ref()
                .and_then(|id| self.work.values().find(|work| &work.id == id))
                .and_then(|work| work.origin.clone())
        });
        let identity = self.next_id("work")?;
        let work = Work {
            id: identity,
            task: field(value, "task_id")?.into(),
            tool: value
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            run: value
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            origin,
            parent,
            kind,
            state: State::Running,
            notification: Notification::Pending,
        };
        self.work.insert(key.clone(), work);
        Ok(key)
    }
    fn snapshot(&mut self, value: &Value) -> Result<Vec<Execution>> {
        let tasks = value
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or(HarnessContractError::InvalidShape)?;
        if tasks.len() > MAX_HARNESS_EXECUTION_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        let mut present = BTreeSet::new();
        let mut events = Vec::new();
        for task in tasks {
            let key = self.ensure_work(task)?;
            if !present.insert(key.clone()) {
                return Err(HarnessContractError::InvalidShape);
            }
            if self.work[&key].state.is_terminal() {
                // A stale snapshot cannot reopen a completed native run.
                self.background_known = false;
            }
            events.push(self.work[&key].event());
        }
        for (key, work) in &mut self.work {
            if !present.contains(key) && !work.state.is_terminal() && work.state != State::Unknown {
                // Disappearance never supplies a terminal outcome or erases a notification.
                work.state = State::Unknown;
                events.push(work.event());
            }
        }
        Ok(events)
    }
    fn inherit_followup_origins(
        followups: &mut BTreeMap<String, Followup>,
        source_id: &str,
        origin: Option<&HarnessAttemptIdentity>,
    ) -> Vec<Execution> {
        let mut events = Vec::new();
        for followup in followups
            .values_mut()
            .filter(|followup| followup.work.as_deref() == Some(source_id))
        {
            if followup.origin.is_none() && origin.is_some() {
                followup.origin = origin.cloned();
                events.push(followup.event());
            }
        }
        events
    }
    fn native_task_state(&mut self, value: &Value) -> State {
        match value
            .get("status")
            .or_else(|| value.pointer("/patch/status"))
            .and_then(Value::as_str)
        {
            Some("completed") => State::Completed,
            Some("failed") => State::Failed,
            Some("stopped" | "killed") => State::Interrupted,
            Some("running") | None => State::Running,
            Some(_) => {
                self.background_known = false;
                State::Unknown
            }
        }
    }
    fn task(&mut self, value: &Value) -> Result<Vec<Execution>> {
        let key = self.ensure_work(value)?;
        let next = self.native_task_state(value);
        if let Some(tool) = value.get("tool_use_id").and_then(Value::as_str) {
            self.pending_background_tools.remove(tool);
        }
        let parent = value
            .get("parent_task_id")
            .and_then(Value::as_str)
            .and_then(|task| {
                let candidates: Vec<_> = self
                    .work
                    .values()
                    .filter(|work| work.task == task)
                    .collect();
                (candidates.len() == 1).then(|| candidates[0].id.clone())
            });
        let inherited_origin = parent
            .as_ref()
            .and_then(|id| self.work.values().find(|work| &work.id == id))
            .and_then(|work| work.origin.clone());
        let work = self
            .work
            .get_mut(&key)
            .ok_or(HarnessContractError::InvalidShape)?;
        if let Some(parent) = parent {
            if parent == work.id || work.parent.as_ref().is_some_and(|old| old != &parent) {
                return Err(HarnessContractError::InvalidShape);
            }
            work.parent = Some(parent);
            if work.origin.is_none() {
                work.origin = inherited_origin;
            }
        }
        if let Some(kind) = value.get("task_type").and_then(Value::as_str) {
            let incoming = match kind {
                "local_bash" => Kind::Command,
                "local_agent" | "local_workflow" => Kind::Subagent,
                _ => Kind::Other,
            };
            if work.kind == Kind::Other {
                work.kind = incoming;
            } else if incoming != Kind::Other && work.kind != incoming {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        if let Some(origin) = value
            .get("tool_use_id")
            .and_then(Value::as_str)
            .and_then(|id| self.tools.get(id))
            .cloned()
            .flatten()
        {
            if work.origin.as_ref().is_some_and(|old| old != &origin) {
                return Err(HarnessContractError::InvalidShape);
            }
            work.origin = Some(origin);
        }
        if !work.state.is_terminal() {
            work.state = next;
        } else if next.is_terminal() && next != work.state {
            return Err(HarnessContractError::InvalidShape);
        }
        let notification =
            value.get("subtype").and_then(Value::as_str) == Some("task_notification");
        if notification {
            work.notification = Notification::Observed;
        }
        let retired_origin = work.origin.clone();
        let source_id = work.id.clone();
        let mut events = vec![work.event()];
        events.extend(Self::inherit_followup_origins(
            &mut self.followups,
            &source_id,
            retired_origin.as_ref(),
        ));
        if notification {
            let id = format!("claude-followup-{}", work.id);
            if !self.followups.contains_key(&id) {
                if self.followups.len() >= MAX_HARNESS_EXECUTION_ITEMS {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                let followup = Followup {
                    id: id.clone(),
                    origin: work.origin.clone(),
                    work: Some(work.id.clone()),
                    state: State::Pending,
                    result_frame: None,
                    output: None,
                };
                events.push(followup.event());
                self.followups.insert(id, followup);
            }
        }
        Ok(events)
    }
    fn followup_start(&mut self, value: &Value) -> Result<Vec<Execution>> {
        let kind = value.pointer("/origin/kind").and_then(Value::as_str);
        if !matches!(kind, Some("task-notification" | "auto-continuation")) {
            return Ok(Vec::new());
        }
        if kind == Some("task-notification")
            && value.pointer("/origin/producer").and_then(Value::as_str) != Some("session-task")
        {
            self.continuations_known = false;
        }
        let run = value.pointer("/origin/runId").and_then(Value::as_str);
        let matching: Vec<_> = self
            .work
            .values()
            .filter(|work| run.is_some() && work.run.as_deref() == run)
            .collect();
        let source = (matching.len() == 1).then(|| (*matching[0]).clone());
        if source.is_none() {
            self.continuations_known = false;
            // An unbound result cannot reuse the active wake's identity. Keep B
            // unresolved; C is uncertainty, not B's successful terminal outcome.
            if value.get("type").and_then(Value::as_str) == Some("result")
                || self.active_followup.is_some()
            {
                return Ok(Vec::new());
            }
        }
        let id = match &source {
            Some(work) => format!("claude-followup-{}", work.id),
            None => self
                .active_followup
                .clone()
                .map_or_else(|| self.next_id("followup-unbound"), Ok)?,
        };
        if let Some(active) = &self.active_followup {
            if source.is_none() {
                return Ok(Vec::new());
            }
            if active != &id {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        if !self.followups.contains_key(&id) {
            if self.followups.len() >= MAX_HARNESS_EXECUTION_ITEMS {
                return Err(HarnessContractError::BoundsExceeded);
            }
            let followup = Followup {
                id: id.clone(),
                origin: source.as_ref().and_then(|w| w.origin.clone()),
                work: source.as_ref().map(|w| w.id.clone()),
                state: State::Pending,
                result_frame: None,
                output: None,
            };
            self.followups.insert(id.clone(), followup);
        }
        let followup = self
            .followups
            .get_mut(&id)
            .ok_or(HarnessContractError::InvalidShape)?;
        if followup.state.is_terminal() {
            self.continuations_known = false;
            return Ok(Vec::new());
        }
        followup.state = State::Running;
        self.active_followup = Some(id);
        self.current = None;
        Ok(vec![followup.event()])
    }
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(HarnessContractError::InvalidShape)?;
    if text.is_empty()
        || text.len() > 256
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(HarnessContractError::InvalidIdentity);
    }
    Ok(text)
}
fn output(value: &Value) -> Result<Option<String>> {
    value
        .get("result")
        .map(|text| {
            let text = text.as_str().ok_or(HarnessContractError::InvalidShape)?;
            if text.len() > MAX_HARNESS_TEXT_BYTES {
                return Err(HarnessContractError::BoundsExceeded);
            }
            Ok(text.to_owned())
        })
        .transpose()
}
fn disposition(value: &Value) -> Result<Disposition> {
    let error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .ok_or(HarnessContractError::InvalidShape)?;
    match value.get("subtype").and_then(Value::as_str) {
        Some("success") if !error => Ok(Disposition::Completed),
        Some(
            "success"
            | "error_during_execution"
            | "error_max_turns"
            | "error_max_budget_usd"
            | "error_max_structured_output_retries",
        ) if error => Ok(Disposition::Failed),
        _ => Err(HarnessContractError::InvalidShape),
    }
}
