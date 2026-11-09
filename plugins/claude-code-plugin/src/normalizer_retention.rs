//! Bounded staged native evidence. Whole CLI snapshots are positive-only pages,
//! not native cursor traversal or an assertion that omitted obligations ended.
#[cfg(test)]
#[path = "normalizer_retention_tests.rs"]
mod tests;

use super::{
    Event, HarnessContractError, HarnessObservation, HarnessObservationKind, NativeMessage,
    Normalizer, Value, json,
};
use bcode_harness_models::{
    HarnessHydrationReply, HarnessHydrationRequest, HarnessInventoryConsistency,
    HarnessInventoryPage, MAX_HARNESS_WIRE_BYTES,
};
use std::collections::VecDeque;

pub(super) struct Staging {
    enabled: bool,
    events: VecDeque<Event>,
    bytes: usize,
    serial: u64,
    request: Option<HarnessHydrationRequest>,
    last_reply: Option<HarnessHydrationReply>,
    revision: u64,
    ready: bool,
    notices: Vec<HarnessObservation>,
    inventory: Option<HarnessInventoryPage>,
}
impl Staging {
    pub const fn new() -> Self {
        Self {
            enabled: false,
            events: VecDeque::new(),
            bytes: 0,
            serial: 0,
            request: None,
            last_reply: None,
            revision: 0,
            ready: false,
            notices: Vec::new(),
            inventory: None,
        }
    }
    pub fn push(&mut self, event: Event) -> Result<(), HarnessContractError> {
        let size = event_bytes(&event);
        if self.bytes.saturating_add(size) > MAX_HARNESS_WIRE_BYTES {
            return Err(HarnessContractError::BoundsExceeded);
        }
        self.bytes += size;
        if let Event::Message(NativeMessage::System(value)) = &event
            && value.get("subtype").and_then(Value::as_str) == Some("background_tasks_changed")
        {
            let tasks = value
                .get("tasks")
                .and_then(Value::as_array)
                .ok_or(HarnessContractError::InvalidShape)?;
            self.serial = self
                .serial
                .checked_add(1)
                .ok_or(HarnessContractError::BoundsExceeded)?;
            let scan = format!("claude-snapshot-{}", self.serial);
            let mut staged = VecDeque::new();
            let mut expanded_bytes = 0_usize;
            for (index, task) in tasks.iter().enumerate() {
                if !task.is_object() {
                    return Err(HarnessContractError::InvalidShape);
                }
                let mut task = task.clone();
                task["type"] = json!("system");
                task["subtype"] = json!("task_started");
                // Explicit positive inventory entry, never a native start edge.
                task["claude_inventory"] = json!({"scan_id":scan,"page_index":index,
                    "next_cursor": Option::<String>::None});
                let event = Event::Message(NativeMessage::System(task));
                expanded_bytes = expanded_bytes.saturating_add(event_bytes(&event));
                if self
                    .bytes
                    .saturating_sub(size)
                    .saturating_add(expanded_bytes)
                    > MAX_HARNESS_WIRE_BYTES
                {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                staged.push_back(event);
            }
            if tasks.is_empty() {
                let mut boundary = value.clone();
                boundary["subtype"] = json!("claude_inventory_boundary");
                boundary["claude_inventory"] = json!({"scan_id":scan,"page_index":0});
                let event = Event::Message(NativeMessage::System(boundary));
                expanded_bytes = event_bytes(&event);
                if self
                    .bytes
                    .saturating_sub(size)
                    .saturating_add(expanded_bytes)
                    > MAX_HARNESS_WIRE_BYTES
                {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                staged.push_back(event);
            }
            self.bytes = self
                .bytes
                .saturating_sub(size)
                .saturating_add(expanded_bytes);
            self.events.extend(staged);
            return Ok(());
        }
        self.events.push_back(event);
        Ok(())
    }
}
impl Normalizer {
    pub const fn enable_retention(&mut self) {
        self.staging.enabled = true;
    }
    pub fn hydration_requests(&self) -> Vec<HarnessHydrationRequest> {
        self.staging.request.iter().cloned().collect()
    }
    pub fn inventory_page(&self) -> Option<HarnessInventoryPage> {
        self.staging.inventory.clone()
    }
    pub fn acknowledge_record(&mut self, observation: &HarnessObservation) {
        self.execution.acknowledge_record(observation);
    }
    pub fn apply_hydration(
        &mut self,
        replies: &[HarnessHydrationReply],
    ) -> Result<(), HarnessContractError> {
        if replies.len() > 1 {
            return Err(HarnessContractError::BoundsExceeded);
        }
        for reply in replies {
            if self.staging.last_reply.as_ref() == Some(reply) {
                continue;
            }
            let request = self
                .staging
                .request
                .as_ref()
                .ok_or(HarnessContractError::InvalidShape)?;
            if request.request_id != reply.request_id || request.reference != reply.reference {
                return Err(HarnessContractError::WrongOwner);
            }
            reply.validate_for(self.target.session_id(), self.target.generation())?;
            if reply.canonical_revision < self.staging.revision {
                return Err(HarnessContractError::InvalidShape);
            }
            if matches!(
                reply.resolution,
                bcode_harness_models::HarnessHydrationResolution::Unavailable
            ) {
                self.execution.lose_coverage();
                if self.staging.notices.is_empty() {
                    let events = self.execution.coverage();
                    self.staging.notices = events
                        .into_iter()
                        .filter_map(|event| {
                            self.observation(HarnessObservationKind::Execution { event })
                        })
                        .collect();
                }
                // Keep the exact pending lookup and staged native record for retry.
                continue;
            }
            if matches!(
                reply.resolution,
                bcode_harness_models::HarnessHydrationResolution::Absent
            ) && self
                .staging
                .events
                .front()
                .is_some_and(|event| match event {
                    Event::Message(NativeMessage::System(value)) => {
                        value.get("task_type").and_then(Value::as_str).is_none()
                    }
                    _ => true,
                })
            {
                // Current reference namespaces cannot prove absence for an
                // untyped task or a run-only wake. Wait for authoritative hydration.
                self.execution.lose_coverage();
                continue;
            }
            self.execution.hydrate(reply)?;
            self.staging.revision = reply.canonical_revision;
            self.staging.last_reply = Some(reply.clone());
            self.staging.request = None;
            self.staging.ready = true;
        }
        Ok(())
    }
    pub fn pump(&mut self) -> Vec<HarnessObservation> {
        if !self.staging.enabled {
            return Vec::new();
        }
        // At most one native record per call; `pump_batch` repeats until evidence needs
        // acknowledgement. Native readers keep draining into a byte-bounded staging
        // buffer; unacknowledged cache is fixed.
        if self.execution.has_unacknowledged() && self.staging.notices.is_empty() {
            return Vec::new();
        }
        if !self.staging.notices.is_empty() {
            return std::mem::take(&mut self.staging.notices);
        }
        if self.staging.request.is_some() {
            return Vec::new();
        }
        let Some(event) = self.staging.events.front() else {
            return Vec::new();
        };
        if !self.staging.ready
            && let Some(reference) = self.execution.native_reference(event)
        {
            self.staging.serial = match self.staging.serial.checked_add(1) {
                Some(serial) => serial,
                None => return self.gap(),
            };
            self.staging.request = Some(HarnessHydrationRequest {
                request_id: format!("claude-hydrate-{}", self.staging.serial),
                reference,
            });
            return Vec::new();
        }
        self.staging.ready = false;
        let Some(event) = self.staging.events.pop_front() else {
            return Vec::new();
        };
        self.staging.bytes = self.staging.bytes.saturating_sub(event_bytes(&event));
        if self.staging.events.is_empty() {
            self.staging.bytes = 0;
        }
        if let Event::Message(NativeMessage::System(value)) = &event
            && let Some(inventory) = value.get("claude_inventory")
        {
            self.staging.inventory = Some(HarnessInventoryPage {
                scan_id: inventory
                    .get("scan_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
                page_index: inventory
                    .get("page_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                // Claude has no native snapshot cursor; never fabricate one.
                next_cursor: None,
                consistency: HarnessInventoryConsistency::NonAtomic,
            });
        }
        self.event_ready(event)
    }
    /// Pump staged records into one durable page cycle until the observation budget is
    /// used, the record budget is exhausted, or evidence needs host acknowledgement or
    /// hydration. Records without execution evidence (text, partial-message frames,
    /// state markers) do not each require a separate host round trip.
    /// Secret-free pump state for diagnostics: staged records, unacknowledged execution
    /// evidence, and a pending hydration request.
    pub fn pump_state(&self) -> (usize, bool, bool) {
        (
            self.staging.events.len(),
            self.execution.has_unacknowledged(),
            self.staging.request.is_some(),
        )
    }

    pub fn pump_batch(
        &mut self,
        max_observations: usize,
        max_records: usize,
    ) -> Vec<HarnessObservation> {
        let mut observations = Vec::new();
        for _ in 0..max_records {
            if observations.len() >= max_observations {
                break;
            }
            let records = self.staging.events.len();
            let notices = !self.staging.notices.is_empty();
            observations.extend(self.pump());
            let progressed = notices || self.staging.events.len() != records;
            if !progressed || self.execution.has_unacknowledged() || self.staging.request.is_some()
            {
                break;
            }
        }
        observations
    }
    pub fn event(&mut self, event: Event) -> Vec<HarnessObservation> {
        // Live text is transient and offered on the next page regardless of staging.
        self.observe_live(&event);
        if !self.staging.enabled {
            return self.event_ready(event);
        }
        // Preserve permission/control delivery independently of inventory hydration.
        if matches!(
            event,
            Event::Callback { .. } | Event::CallbackCancelled { .. }
        ) {
            return self.event_ready(event);
        }
        if self.staging.push(event).is_err() {
            return self.gap();
        }
        Vec::new()
    }
}
fn event_bytes(event: &Event) -> usize {
    match event {
        Event::Message(
            NativeMessage::System(value)
            | NativeMessage::Assistant(value)
            | NativeMessage::User(value)
            | NativeMessage::Result(value)
            | NativeMessage::Stream(value)
            | NativeMessage::Notice(value),
        ) => serde_json::to_vec(value).map_or(MAX_HARNESS_WIRE_BYTES, |bytes| bytes.len()),
        Event::Attribution(evidence) => evidence
            .native_frame_uuid
            .len()
            .saturating_add(evidence.primary.as_str().len())
            .saturating_add(
                evidence
                    .consumed
                    .iter()
                    .map(|uuid| uuid.as_str().len())
                    .sum::<usize>(),
            )
            .saturating_add(512),
        _ => 256,
    }
}
