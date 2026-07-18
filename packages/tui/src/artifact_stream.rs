//! Generic incremental artifact transport for plugin-owned TUI visuals.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bcode_client::{BcodeClient, ClientError};
use bcode_plugin_sdk::tui::PluginTuiArtifactChunk;
use bcode_session_models::{SessionEventKind, SessionId, ToolInvocationStreamEvent};

const ACTIVE_ARTIFACT_FETCH_BYTES: u32 = 256 * 1024;
const ACTIVE_ARTIFACT_RETRY_BASE: Duration = Duration::from_millis(100);
const ACTIVE_ARTIFACT_RETRY_MAX: Duration = Duration::from_secs(2);

type ActiveArtifactKey = (SessionId, String, String, String);

#[derive(Debug, Clone)]
struct ActiveArtifactTarget {
    producer_plugin_id: String,
    schema: String,
    schema_version: u32,
    content_type: Option<String>,
    committed_bytes: u64,
    revision: u64,
    finalized: bool,
}

#[derive(Debug, Default)]
struct ActiveArtifactFetchState {
    next_offset: u64,
    target: Option<ActiveArtifactTarget>,
    fetching: bool,
    retry_at: Option<Instant>,
    consecutive_failures: u32,
    terminal_error: Option<String>,
}

#[derive(Debug)]
pub struct ActiveArtifactFetchCompletion {
    session_id: SessionId,
    key: ActiveArtifactKey,
    requested_offset: u64,
    target_revision: u64,
    result: Result<bcode_client::SessionArtifactRange, ClientError>,
}

fn active_artifact_completion_is_current(
    completion_session_id: SessionId,
    current_session_id: Option<SessionId>,
    requested_offset: u64,
    next_offset: u64,
) -> bool {
    Some(completion_session_id) == current_session_id && requested_offset == next_offset
}

fn validate_active_artifact_range(
    range: &bcode_client::SessionArtifactRange,
    next_offset: u64,
    target: &ActiveArtifactTarget,
    requested_revision: u64,
) -> Result<u64, &'static str> {
    let expected_end = range.next_offset();
    if range.offset != next_offset
        || range.total_bytes < expected_end
        || range.total_bytes > target.committed_bytes
        || requested_revision > target.revision
        || range.reference_revision < requested_revision
    {
        return Err("artifact range response did not match the requested committed prefix");
    }
    if range.bytes.is_empty() && next_offset < target.committed_bytes {
        return Err("artifact range response ended before the committed boundary");
    }
    Ok(expected_end)
}

pub struct ArtifactStreamCoordinator {
    artifact_fetches: BTreeMap<ActiveArtifactKey, ActiveArtifactFetchState>,
    artifact_fetch_sender: tokio::sync::mpsc::UnboundedSender<ActiveArtifactFetchCompletion>,
    artifact_fetch_receiver: tokio::sync::mpsc::UnboundedReceiver<ActiveArtifactFetchCompletion>,
    passive_client: BcodeClient,
}

impl ArtifactStreamCoordinator {
    pub(crate) fn new(passive_client: BcodeClient) -> Self {
        let (artifact_fetch_sender, artifact_fetch_receiver) =
            tokio::sync::mpsc::unbounded_channel();
        Self {
            artifact_fetches: BTreeMap::new(),
            artifact_fetch_sender,
            artifact_fetch_receiver,
            passive_client,
        }
    }

    pub(crate) fn retain_session(&mut self, session_id: Option<SessionId>) {
        self.artifact_fetches
            .retain(|key, _| Some(key.0) == session_id);
    }

    pub(crate) fn observe_finalized_artifact(
        &mut self,
        session_id: SessionId,
        sequence: u64,
        event: &SessionEventKind,
    ) {
        let SessionEventKind::ToolCallFinished {
            tool_call_id,
            semantic_result: Some(bcode_session_models::ToolInvocationResult::Artifact { artifact }),
            ..
        } = event
        else {
            return;
        };
        for reference in &artifact.refs {
            let Some(committed_bytes) = reference.byte_len else {
                continue;
            };
            let availability = reference
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("availability"))
                .and_then(serde_json::Value::as_str);
            if matches!(
                availability,
                Some("missing" | "incomplete" | "corrupt" | "evicted" | "unavailable")
            ) {
                continue;
            }
            let key = (
                session_id,
                tool_call_id.clone(),
                artifact.artifact_id.clone(),
                reference.key.clone(),
            );
            let state = self.artifact_fetches.entry(key.clone()).or_default();
            if state
                .target
                .as_ref()
                .is_some_and(|target| sequence < target.revision)
            {
                continue;
            }
            state.terminal_error = None;
            state.target = Some(ActiveArtifactTarget {
                producer_plugin_id: artifact.producer_plugin_id.clone(),
                schema: artifact.schema.clone(),
                schema_version: artifact.schema_version,
                content_type: reference.content_type.clone(),
                committed_bytes,
                revision: sequence,
                finalized: true,
            });
            self.schedule_active_artifact_fetch(session_id, &key);
        }
    }

    pub(crate) fn observe_live_event(
        &mut self,
        session_id: SessionId,
        event: &ToolInvocationStreamEvent,
    ) {
        let ToolInvocationStreamEvent::ArtifactUpdate {
            tool_call_id,
            artifact_id,
            reference_key,
            producer_plugin_id,
            schema,
            schema_version,
            content_type,
            committed_bytes,
            revision,
            availability,
            finalized,
            ..
        } = event
        else {
            return;
        };
        let key = (
            session_id,
            tool_call_id.clone(),
            artifact_id.clone(),
            reference_key.clone(),
        );
        let state = self.artifact_fetches.entry(key.clone()).or_default();
        if availability.as_deref() == Some("incomplete") {
            state.fetching = false;
            state.retry_at = None;
            state.terminal_error = Some(
                "active artifact is incomplete because its producer stopped before finalization"
                    .to_owned(),
            );
            return;
        }
        if state
            .target
            .as_ref()
            .is_some_and(|target| *revision <= target.revision)
        {
            return;
        }
        state.target = Some(ActiveArtifactTarget {
            producer_plugin_id: producer_plugin_id.clone(),
            schema: schema.clone(),
            schema_version: *schema_version,
            content_type: content_type.clone(),
            committed_bytes: *committed_bytes,
            revision: *revision,
            finalized: *finalized,
        });
        self.schedule_active_artifact_fetch(session_id, &key);
    }

    fn schedule_active_artifact_fetch(&mut self, session_id: SessionId, key: &ActiveArtifactKey) {
        let Some(state) = self.artifact_fetches.get_mut(key) else {
            return;
        };
        let Some(target) = state.target.as_ref() else {
            return;
        };
        if state.fetching
            || state.next_offset >= target.committed_bytes
            || state.terminal_error.is_some()
            || state
                .retry_at
                .is_some_and(|retry_at| retry_at > Instant::now())
        {
            return;
        }
        let requested_offset = state.next_offset;
        let remaining = target.committed_bytes.saturating_sub(requested_offset);
        let length = u32::try_from(remaining)
            .unwrap_or(u32::MAX)
            .min(ACTIVE_ARTIFACT_FETCH_BYTES);
        let target_revision = target.revision;
        state.fetching = true;
        state.retry_at = None;
        let client = self.passive_client.clone();
        let sender = self.artifact_fetch_sender.clone();
        let task_key = key.clone();
        tokio::spawn(async move {
            let result = client
                .session_artifact_range(
                    session_id,
                    task_key.2.clone(),
                    task_key.3.clone(),
                    requested_offset,
                    length,
                )
                .await;
            let _ = sender.send(ActiveArtifactFetchCompletion {
                session_id,
                key: task_key,
                requested_offset,
                target_revision,
                result,
            });
        });
    }

    #[allow(clippy::too_many_lines)] // Keeps response validation, delivery, retry, and scheduling as one state transition.
    pub(crate) fn handle_completion(
        &mut self,
        current_session_id: Option<SessionId>,
        completion: ActiveArtifactFetchCompletion,
        deliver: impl FnOnce(&PluginTuiArtifactChunk) -> Result<bool, String>,
    ) -> bool {
        let key = completion.key.clone();
        let chunk = {
            let Some(state) = self.artifact_fetches.get_mut(&key) else {
                return false;
            };
            if !active_artifact_completion_is_current(
                completion.session_id,
                current_session_id,
                completion.requested_offset,
                state.next_offset,
            ) {
                return false;
            }
            state.fetching = false;
            let range = match completion.result {
                Ok(range) => range,
                Err(error) => {
                    Self::defer_active_artifact_fetch(state, error.to_string());
                    return false;
                }
            };
            let Some(target) = state.target.clone() else {
                return false;
            };
            let expected_end = match validate_active_artifact_range(
                &range,
                state.next_offset,
                &target,
                completion.target_revision,
            ) {
                Ok(expected_end) => expected_end,
                Err(error) => {
                    Self::defer_active_artifact_fetch(state, error.to_owned());
                    return false;
                }
            };
            if range.bytes.is_empty() {
                state.consecutive_failures = 0;
                state.retry_at = None;
                None
            } else {
                Some((
                    PluginTuiArtifactChunk {
                        tool_call_id: key.1.clone(),
                        artifact_id: key.2.clone(),
                        reference_key: key.3.clone(),
                        producer_plugin_id: target.producer_plugin_id,
                        schema: target.schema,
                        schema_version: target.schema_version,
                        content_type: target.content_type,
                        offset: range.offset,
                        total_bytes: range.total_bytes,
                        revision: range.reference_revision,
                        finalized: range.finalized || target.finalized,
                        bytes: range.bytes,
                    },
                    expected_end,
                ))
            }
        };

        let mut redraw = false;
        if let Some((chunk, expected_end)) = chunk {
            let delivery = deliver(&chunk);
            let state = self
                .artifact_fetches
                .get_mut(&key)
                .expect("artifact fetch state remains registered during delivery");
            match delivery {
                Ok(true) => {
                    state.next_offset = expected_end;
                    state.consecutive_failures = 0;
                    state.retry_at = None;
                    redraw = true;
                }
                Ok(false) => {
                    state.terminal_error =
                        Some("artifact schema has no owning visual adapter".to_owned());
                }
                Err(error) => state.terminal_error = Some(error),
            }
        }
        self.schedule_active_artifact_fetch(completion.session_id, &key);
        redraw
    }

    pub(crate) fn start_due_fetches(&mut self, now: Instant) {
        let due = self
            .artifact_fetches
            .iter()
            .filter(|(_, state)| {
                !state.fetching
                    && state.terminal_error.is_none()
                    && state.retry_at.is_some_and(|retry_at| retry_at <= now)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in due {
            self.schedule_active_artifact_fetch(key.0, &key);
        }
    }

    pub(crate) fn next_retry_at(&self) -> Option<Instant> {
        self.artifact_fetches
            .values()
            .filter_map(|state| state.retry_at)
            .min()
    }

    pub(crate) async fn next_completion(&mut self) -> Option<ActiveArtifactFetchCompletion> {
        self.artifact_fetch_receiver.recv().await
    }

    fn defer_active_artifact_fetch(state: &mut ActiveArtifactFetchState, _error: String) {
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        let exponent = state.consecutive_failures.saturating_sub(1).min(4);
        let multiplier = 1_u32 << exponent;
        let delay = ACTIVE_ARTIFACT_RETRY_BASE
            .saturating_mul(multiplier)
            .min(ACTIVE_ARTIFACT_RETRY_MAX);
        state.retry_at = Some(Instant::now() + delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(committed_bytes: u64, revision: u64, finalized: bool) -> ActiveArtifactTarget {
        ActiveArtifactTarget {
            producer_plugin_id: "test.producer".to_owned(),
            schema: "test.artifact".to_owned(),
            schema_version: 1,
            content_type: Some("application/octet-stream".to_owned()),
            committed_bytes,
            revision,
            finalized,
        }
    }

    fn range(
        offset: u64,
        total_bytes: u64,
        revision: u64,
        bytes: &[u8],
    ) -> bcode_client::SessionArtifactRange {
        bcode_client::SessionArtifactRange {
            artifact_id: "artifact".to_owned(),
            reference_key: "reference".to_owned(),
            content_type: Some("application/octet-stream".to_owned()),
            offset,
            total_bytes,
            reference_bytes: Some(total_bytes),
            reference_revision: revision,
            finalized: false,
            finalized_event_seq: None,
            availability: Some("active".to_owned()),
            complete: Some(false),
            checksum_sha256: None,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn range_validation_accepts_contiguous_growth_and_rejects_duplicates() {
        let active = target(10, 2, false);
        assert_eq!(
            validate_active_artifact_range(&range(0, 10, 2, b"abc"), 0, &active, 2),
            Ok(3)
        );
        let session_id = SessionId::new();
        assert!(active_artifact_completion_is_current(
            session_id,
            Some(session_id),
            0,
            0
        ));
        assert!(!active_artifact_completion_is_current(
            session_id,
            Some(session_id),
            0,
            3
        ));
    }

    #[test]
    fn failed_fetch_exposes_an_independent_retry_deadline() {
        let client = BcodeClient::default_endpoint();
        let mut coordinator = ArtifactStreamCoordinator::new(client);
        let session_id = SessionId::new();
        let key = (
            session_id,
            "tool".to_owned(),
            "artifact".to_owned(),
            "reference".to_owned(),
        );
        let mut state = ActiveArtifactFetchState {
            target: Some(target(3, 1, false)),
            ..ActiveArtifactFetchState::default()
        };
        ArtifactStreamCoordinator::defer_active_artifact_fetch(
            &mut state,
            "unavailable".to_owned(),
        );
        coordinator.artifact_fetches.insert(key, state);
        assert!(coordinator.next_retry_at().is_some());
    }

    #[test]
    fn accepted_chunk_advances_only_after_generic_adapter_delivery() {
        let client = BcodeClient::default_endpoint();
        let mut coordinator = ArtifactStreamCoordinator::new(client);
        let session_id = SessionId::new();
        let key = (
            session_id,
            "tool".to_owned(),
            "artifact".to_owned(),
            "reference".to_owned(),
        );
        coordinator.artifact_fetches.insert(
            key.clone(),
            ActiveArtifactFetchState {
                target: Some(target(3, 1, false)),
                fetching: true,
                ..ActiveArtifactFetchState::default()
            },
        );
        let completion = ActiveArtifactFetchCompletion {
            session_id,
            key: key.clone(),
            requested_offset: 0,
            target_revision: 1,
            result: Ok(range(0, 3, 1, b"abc")),
        };
        let changed = coordinator.handle_completion(Some(session_id), completion, |chunk| {
            assert_eq!(chunk.producer_plugin_id, "test.producer");
            assert_eq!(chunk.schema, "test.artifact");
            assert_eq!(chunk.bytes, b"abc");
            Ok(true)
        });
        assert!(changed);
        assert_eq!(
            coordinator
                .artifact_fetches
                .get(&key)
                .expect("artifact state")
                .next_offset,
            3
        );
    }
}
