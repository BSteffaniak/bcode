//! Native consumed-prompt correlation. These keys are never native turn identifiers.
use crate::{Error, validate_session_uuid};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Client-selected UUID for one submission attempt on one live transport.
/// Reusing it does not authorize replay after an uncertain write or process loss.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SubmissionUuid(String);
impl SubmissionUuid {
    /// Validate a canonical, lower-case UUID without inventing a native turn identity.
    /// # Errors
    /// Returns `Protocol` for a noncanonical UUID.
    pub fn new(value: String) -> Result<Self, Error> {
        validate_session_uuid(&value)?;
        Ok(Self(value))
    }
    /// Native wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the native frame proves, not settlement or assignment completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributionStage {
    /// A main-thread reply explicitly names the prompts consumed so far.
    Reply,
    /// A native result explicitly names the prompts it answers. Further work may follow.
    Result {
        /// Native error flag, not a host assignment outcome.
        is_error: bool,
        /// Pending user sends only; zero excludes neither system work nor discarded sends.
        queued_turn_count: Option<u64>,
    },
}

/// Validated native correlation with retained client attempts.
/// Session/connection/ownership fencing remains the caller's responsibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmissionAttribution {
    /// Native message envelope UUID for deduplication, never a native turn ID.
    pub native_frame_uuid: String,
    /// Last triggering prompt, as explicitly identified by the native producer.
    pub primary: SubmissionUuid,
    /// Every consumed prompt named by this frame, in consumption order (at most 64).
    pub consumed: Vec<SubmissionUuid>,
    /// Resumed worker evidence must not be admitted as a fresh attempt automatically.
    pub resumed: bool,
    /// Reply consumption and result delivery are separate evidence classes.
    pub stage: AttributionStage,
}

/// Bounded process-local evidence. No eviction can make an old UUID reusable.
/// A full ledger requires a new transport, not an automatic resend.
pub struct AttributionLedger {
    submissions: BTreeSet<SubmissionUuid>,
    frames: BTreeMap<String, SubmissionAttribution>,
    current: Option<SubmissionUuid>,
}
impl AttributionLedger {
    pub(crate) const MAX_SUBMISSIONS: usize = 256;
    const MAX_FRAMES: usize = 1024;

    pub(crate) const fn new() -> Self {
        Self {
            submissions: BTreeSet::new(),
            frames: BTreeMap::new(),
            current: None,
        }
    }

    pub(crate) fn retain(&mut self, uuid: SubmissionUuid) -> Result<(), Error> {
        if self.submissions.contains(&uuid) {
            return Err(Error::Protocol);
        }
        if self.submissions.len() >= Self::MAX_SUBMISSIONS {
            return Err(Error::Capacity);
        }
        self.current = Some(uuid.clone());
        self.submissions.insert(uuid);
        Ok(())
    }

    pub(crate) fn answers_current(&self, attribution: Option<&SubmissionAttribution>) -> bool {
        attribution.is_some_and(|evidence| {
            matches!(evidence.stage, AttributionStage::Result { .. })
                && !evidence.resumed
                && self
                    .current
                    .as_ref()
                    .is_some_and(|uuid| evidence.consumed.contains(uuid))
        })
    }

    pub(crate) fn observe(
        &mut self,
        value: &Value,
    ) -> Result<Option<SubmissionAttribution>, Error> {
        if self.submissions.is_empty() {
            return Ok(None);
        }
        let kind = value.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("assistant" | "stream_event" | "result"))
            || value
                .get("parent_tool_use_id")
                .is_some_and(|id| !id.is_null())
        {
            return Ok(None);
        }
        let Some(primary) = value.get("user_message_uuid") else {
            return if value.get("user_message_uuids").is_some() {
                Err(Error::Protocol)
            } else {
                Ok(None)
            };
        };
        let primary = parse_uuid(primary)?;
        let consumed = if let Some(values) = value.get("user_message_uuids") {
            let values = values.as_array().ok_or(Error::Protocol)?;
            if values.is_empty() || values.len() > 64 {
                return Err(Error::Protocol);
            }
            values
                .iter()
                .map(parse_uuid)
                .collect::<Result<Vec<_>, _>>()?
        } else {
            vec![primary.clone()]
        };
        let unique: BTreeSet<_> = consumed.iter().collect();
        if unique.len() != consumed.len()
            || !consumed.contains(&primary)
            || consumed.iter().any(|uuid| !self.submissions.contains(uuid))
        {
            return Err(Error::Protocol);
        }
        let resumed = match value.get("resume_reason") {
            None => false,
            Some(Value::String(reason)) if !reason.is_empty() && reason.len() <= 256 => true,
            Some(_) => return Err(Error::Protocol),
        };
        let stage = if kind == Some("result") {
            let queued_turn_count = value
                .get("queued_turn_count")
                .map(|count| count.as_u64().ok_or(Error::Protocol))
                .transpose()?;
            AttributionStage::Result {
                is_error: value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .ok_or(Error::Protocol)?,
                queued_turn_count,
            }
        } else {
            AttributionStage::Reply
        };
        let frame_id = value
            .get("uuid")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        validate_session_uuid(frame_id)?;
        let attribution = SubmissionAttribution {
            native_frame_uuid: frame_id.to_owned(),
            primary,
            consumed,
            resumed,
            stage,
        };
        if let Some(previous) = self.frames.get(frame_id) {
            return if previous == &attribution {
                Ok(None)
            } else {
                Err(Error::Protocol)
            };
        }
        if self.frames.len() >= Self::MAX_FRAMES {
            return Err(Error::Capacity);
        }
        self.frames.insert(frame_id.to_owned(), attribution.clone());
        Ok(Some(attribution))
    }
}
fn parse_uuid(value: &Value) -> Result<SubmissionUuid, Error> {
    SubmissionUuid::new(value.as_str().ok_or(Error::Protocol)?.to_owned())
}

/// Decode a bounded native JSON object without last-key-wins correlation ambiguity.
/// Nested content remains native payload; all top-level duplicate fields are rejected.
pub fn decode_frame(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    struct Frame(Value);
    impl<'de> serde::Deserialize<'de> for Frame {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Frame;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a native frame with unique top-level fields")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Frame, M::Error> {
                    let mut fields = serde_json::Map::new();
                    while let Some((key, value)) = map.next_entry::<String, Value>()? {
                        if fields.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom("duplicate native frame field"));
                        }
                    }
                    Ok(Frame(Value::Object(fields)))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    serde_json::from_slice::<Frame>(bytes).map(|frame| frame.0)
}

#[cfg(test)]
mod tests {
    use super::{AttributionLedger, AttributionStage, SubmissionUuid, decode_frame};
    use crate::Error;
    use serde_json::{Value, json};

    const A: &str = "00000000-0000-4000-8000-000000000001";
    const B: &str = "00000000-0000-4000-8000-000000000002";
    const FRAME: &str = "00000000-0000-4000-8000-000000000003";

    fn ledger() -> AttributionLedger {
        let mut ledger = AttributionLedger::new();
        ledger
            .retain(SubmissionUuid::new(A.into()).unwrap())
            .unwrap();
        ledger
            .retain(SubmissionUuid::new(B.into()).unwrap())
            .unwrap();
        ledger
    }
    fn reply() -> Value {
        json!({"type":"assistant","uuid":FRAME,"user_message_uuid":A})
    }
    #[test]
    fn missing_is_not_acceptance_and_malformed_is_not_missing() {
        let mut ledger = ledger();
        assert_eq!(ledger.observe(&json!({"type":"result"})).unwrap(), None);
        for value in [json!(null), json!(12), json!("not-a-uuid")] {
            let mut frame = reply();
            frame["user_message_uuid"] = value;
            assert!(ledger.observe(&frame).is_err());
        }
        assert!(
            ledger
                .observe(&json!({"type":"assistant","user_message_uuids":[A]}))
                .is_err()
        );
    }
    #[test]
    fn merged_prompts_and_singular_fallback_are_explicit() {
        let mut retained = ledger();
        let mut frame = reply();
        frame["user_message_uuids"] = json!([B, A]);
        let evidence = retained.observe(&frame).unwrap().unwrap();
        assert_eq!(
            evidence
                .consumed
                .iter()
                .map(SubmissionUuid::as_str)
                .collect::<Vec<_>>(),
            [B, A]
        );
        assert_eq!(evidence.primary.as_str(), A);
        assert_eq!(evidence.stage, AttributionStage::Reply);
        let evidence = ledger().observe(&reply()).unwrap().unwrap();
        assert_eq!(evidence.consumed, vec![evidence.primary.clone()]);
    }
    #[test]
    fn conflicts_unknowns_duplicates_and_oversized_lists_fail_closed() {
        for values in [
            json!([B]),
            json!([A, A]),
            json!([]),
            json!([A, FRAME]),
            json!(vec![A; 65]),
        ] {
            let mut frame = reply();
            frame["user_message_uuids"] = values;
            assert!(ledger().observe(&frame).is_err());
        }
        let mut ledger = ledger();
        let mut frame = reply();
        assert!(ledger.observe(&frame).unwrap().is_some());
        assert!(ledger.observe(&frame).unwrap().is_none());
        frame["user_message_uuid"] = json!(B);
        assert!(ledger.observe(&frame).is_err());
    }
    #[test]
    fn result_and_resume_do_not_mean_settlement() {
        let mut frame = reply();
        frame["type"] = json!("result");
        frame["is_error"] = json!(false);
        frame["queued_turn_count"] = json!(2);
        frame["resume_reason"] = json!("interrupted_turn");
        let evidence = ledger().observe(&frame).unwrap().unwrap();
        assert!(evidence.resumed);
        assert_eq!(
            evidence.stage,
            AttributionStage::Result {
                is_error: false,
                queued_turn_count: Some(2)
            }
        );
        frame["queued_turn_count"] = json!(-1);
        assert!(ledger().observe(&frame).is_err());
    }
    #[test]
    fn subagent_and_replay_are_not_consumption_proof() {
        let mut frame = reply();
        frame["parent_tool_use_id"] = json!("tool");
        assert!(ledger().observe(&frame).unwrap().is_none());
        frame["type"] = json!("user");
        frame["parent_tool_use_id"] = Value::Null;
        assert!(ledger().observe(&frame).unwrap().is_none());
    }
    #[test]
    fn duplicate_wire_fields_fail_before_value_parsing() {
        assert!(
            decode_frame(
                br#"{"type":"assistant","user_message_uuid":"one","user_message_uuid":"two"}"#
            )
            .is_err()
        );
        assert!(decode_frame(br#"{"type":"result","type":"assistant"}"#).is_err());
        assert!(decode_frame(br#"{"type":"assistant","message":{"content":[]}}"#).is_ok());
    }

    #[test]
    fn only_a_nonresumed_result_answering_current_submission_can_settle() {
        let mut retained = ledger(); // B is the latest retained attempt.
        let mut frame = reply();
        frame["type"] = json!("result");
        frame["is_error"] = json!(false);
        let stale = retained.observe(&frame).unwrap();
        assert!(!retained.answers_current(stale.as_ref()));
        frame["uuid"] = json!(B);
        frame["user_message_uuid"] = json!(B);
        let current = retained.observe(&frame).unwrap();
        assert!(retained.answers_current(current.as_ref()));
        assert!(!retained.answers_current(None));
        frame["uuid"] = json!(A);
        frame["resume_reason"] = json!("interrupted_turn");
        let resumed = retained.observe(&frame).unwrap();
        assert!(!retained.answers_current(resumed.as_ref()));
    }

    #[test]
    fn retained_frame_evidence_has_a_hard_bound() {
        let mut retained = ledger();
        for index in 0..AttributionLedger::MAX_FRAMES {
            let mut frame = reply();
            frame["uuid"] = json!(format!("00000000-0000-4000-8000-{index:012x}"));
            assert!(retained.observe(&frame).unwrap().is_some());
        }
        let mut frame = reply();
        frame["uuid"] = json!("00000000-0000-4000-8000-ffffffffffff");
        assert!(matches!(retained.observe(&frame), Err(Error::Capacity)));
    }

    #[test]
    fn retained_submissions_never_evict_or_allow_reuse() {
        let mut ledger = AttributionLedger::new();
        for n in 0..AttributionLedger::MAX_SUBMISSIONS {
            ledger
                .retain(SubmissionUuid::new(format!("00000000-0000-4000-8000-{n:012x}")).unwrap())
                .unwrap();
        }
        assert!(matches!(
            ledger.retain(SubmissionUuid::new(A.into()).unwrap()),
            Err(Error::Protocol)
        ));
        assert!(matches!(
            ledger.retain(
                SubmissionUuid::new("00000000-0000-4000-8000-ffffffffffff".into()).unwrap()
            ),
            Err(Error::Capacity)
        ));
    }
}
