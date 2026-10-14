//! Content-free request lineage and provider conversion observations.
//!
//! These contracts describe representation, not network delivery, remote acceptance or quality.
//! Producers must validate before use; deserialization alone does not establish compatibility.
//! Request ordinals are scoped to the enclosing session and turn. Consumers must correlate
//! those identities separately and must not accept a receipt from another request envelope.
//! A block reference locates a contribution; it does not certify that all text in that block
//! belongs to the contribution. Providers unable to track the complete contribution use Unknown.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Maximum entries in each independently bounded lineage/receipt envelope.
pub const MAX_CONTEXT_LINEAGE_ENTRIES: usize = 256;

/// Host-issued identities scoped to this exact request representation.
/// Numeric identities must not be derived from content, paths or authored candidate IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContextLineage {
    /// Supported compatibility boundary is version 1. Future versions fail validation.
    pub version: u32,
    /// Host-issued request ordinal; allocate anew after any representation change or retry.
    pub request_ordinal: u64,
    /// Entries whose presence in the normalized request was actually observed by the host.
    #[serde(deserialize_with = "bounded_entries")]
    pub entries: Vec<ContextLineageEntry>,
    /// Number of entries omitted by the observation bound; not evidence of content omission.
    pub omitted: u64,
}

/// One host-observed source associated with an actual normalized content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextLineageEntry {
    /// Unique host-issued source ordinal within this request.
    pub source_ordinal: u64,
    /// Host-issued consumer operation ordinal, not an authored identifier.
    pub operation_ordinal: u64,
    /// Whether the host requires this source to survive conversion.
    pub mandatory: bool,
    /// Zero-based normalized message index.
    pub message_index: usize,
    /// Zero-based content block index within that message.
    pub block_index: usize,
}

/// Provider observation of the actual conversion, never an inference from selection counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContextRepresentation {
    /// Supported compatibility boundary is version 1.
    pub version: u32,
    /// Must equal the host lineage's exact request ordinal.
    pub request_ordinal: u64,
    /// At most one observation per source; missing observations remain unknown.
    #[serde(deserialize_with = "bounded_entries")]
    pub entries: Vec<ContextRepresentationEntry>,
    /// Number of lineage entries omitted from this bounded observation.
    pub omitted: u64,
}

/// One source conversion outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRepresentationEntry {
    /// Refers only to a source in the matching host lineage.
    pub source_ordinal: u64,
    /// Actual conversion observation.
    pub disposition: ContextRepresentationDisposition,
}

/// Unknown must be used when source mapping cannot establish complete representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRepresentationDisposition {
    /// The entire source contribution survives in the converted request.
    Represented,
    /// The source contribution is absent or only partially represented.
    Dropped,
    /// Conversion cannot establish the source contribution's representation.
    Unknown,
}

fn bounded_entries<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Entries<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Entries<T> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded context observation array")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while entries.len() < MAX_CONTEXT_LINEAGE_ENTRIES {
                let Some(entry) = sequence.next_element()? else {
                    return Ok(entries);
                };
                entries.push(entry);
            }
            // Probe one excess element without constructing another domain entry or
            // allocating from the untrusted sequence size hint.
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("oversized context observation"));
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Entries(std::marker::PhantomData))
}

impl RequestContextLineage {
    /// Validate compatibility, bounds, uniqueness and actual normalized block references.
    ///
    /// # Errors
    /// Returns an error for unsupported versions, excess entries, duplicate sources or invalid
    /// message/block references. This does not inspect or certify contribution content.
    pub fn validate(&self, messages: &[crate::ModelMessage]) -> Result<(), &'static str> {
        if self.version != 1 || self.entries.len() > MAX_CONTEXT_LINEAGE_ENTRIES {
            return Err("unsupported or oversized context lineage");
        }
        let mut sources = BTreeSet::new();
        for entry in &self.entries {
            if !sources.insert(entry.source_ordinal)
                || messages
                    .get(entry.message_index)
                    .and_then(|message| message.content.get(entry.block_index))
                    .is_none()
            {
                return Err("duplicate source or invalid context reference");
            }
        }
        Ok(())
    }
}

impl RequestContextRepresentation {
    /// Validate this receipt against the exact, separately validated host lineage.
    ///
    /// # Errors
    /// Returns an error for unsupported versions, foreign request/source identities, excess
    /// entries, duplicate observations or inconsistent omitted accounting.
    pub fn validate(&self, lineage: &RequestContextLineage) -> Result<(), &'static str> {
        if self.version != 1
            || lineage.version != 1
            || self.request_ordinal != lineage.request_ordinal
            || self.entries.len() > MAX_CONTEXT_LINEAGE_ENTRIES
            || lineage.entries.len() > MAX_CONTEXT_LINEAGE_ENTRIES
        {
            return Err("unsupported or foreign context representation");
        }
        let sources: BTreeSet<_> = lineage
            .entries
            .iter()
            .map(|entry| entry.source_ordinal)
            .collect();
        let mut observed = BTreeSet::new();
        if sources.len() != lineage.entries.len() {
            return Err("duplicate lineage source");
        }
        for entry in &self.entries {
            if !sources.contains(&entry.source_ordinal) || !observed.insert(entry.source_ordinal) {
                return Err("foreign or duplicate representation source");
            }
        }
        if usize::try_from(self.omitted).ok() != Some(sources.len() - observed.len()) {
            return Err("inconsistent representation omission accounting");
        }
        Ok(())
    }
}

/// Maximum encoded normalized observation artifact bytes; checked before JSON decoding.
pub const MAX_CONTEXT_REPRESENTATION_OBSERVATION_BYTES: usize = 131_072;

/// Content-free observation bound to an exact canonical producer event and dispatched request.
/// Deserialization alone is not validation, authority, delivery or quality evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRepresentationObservation {
    /// Only version 1 is supported.
    pub version: u32,
    /// Canonical lower-case hyphenated session UUID.
    pub session_id: String,
    /// Canonical enclosing execution turn, distinct from provider round ID.
    pub canonical_turn_id: String,
    /// Exact dispatched `ModelTurnRequest` `turn_id`.
    pub provider_request_id: String,
    /// Canonical `ModelRequestBuilt` producer event sequence.
    pub model_request_sequence: u64,
    /// Exact representation ordinal shared by lineage and receipt.
    pub request_ordinal: u64,
    /// Host-owned content-free normalized lineage.
    pub lineage: RequestContextLineage,
    /// Provider conversion observation, never a delivery claim.
    pub receipt: RequestContextRepresentation,
}

impl ContextRepresentationObservation {
    /// Construct from the exact dispatched request and validated conversion receipt.
    /// # Errors
    /// Rejects absent/invalid lineage, invalid turn identity or a foreign/invalid receipt.
    pub fn new(
        request: &crate::ModelTurnRequest,
        canonical_turn_id: String,
        model_request_sequence: u64,
        receipt: RequestContextRepresentation,
    ) -> Result<Self, &'static str> {
        let lineage = request
            .context_lineage
            .clone()
            .ok_or("request lineage unavailable")?;
        let value = Self {
            version: 1,
            session_id: request.session_id.to_string(),
            canonical_turn_id,
            provider_request_id: request.turn_id.clone(),
            model_request_sequence,
            request_ordinal: lineage.request_ordinal,
            lineage,
            receipt,
        };
        value.validate(request, &value.canonical_turn_id, model_request_sequence)?;
        Ok(value)
    }

    /// Validate against caller-verified canonical identity and the exact originating request.
    /// # Errors
    /// Rejects unsupported versions, invalid UUID/IDs, mismatched producer identities,
    /// mismatched lineage or receipt and invalid normalized block references.
    pub fn validate(
        &self,
        request: &crate::ModelTurnRequest,
        canonical_turn_id: &str,
        model_request_sequence: u64,
    ) -> Result<(), &'static str> {
        let session: bcode_session_models::SessionId = self
            .session_id
            .parse()
            .map_err(|_| "invalid observation session identity")?;
        let valid_id =
            |id: &str| !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control);
        if self.version != 1
            || session.to_string() != self.session_id
            || session != request.session_id
            || !valid_id(&self.canonical_turn_id)
            || !valid_id(&self.provider_request_id)
            || self.canonical_turn_id != canonical_turn_id
            || self.provider_request_id != request.turn_id
            || self.model_request_sequence != model_request_sequence
            || self.request_ordinal == 0
            || self.request_ordinal != self.lineage.request_ordinal
            || self.request_ordinal != self.receipt.request_ordinal
            || request.context_lineage.as_ref() != Some(&self.lineage)
        {
            return Err("unsupported or foreign context observation");
        }
        self.lineage.validate(&request.messages)?;
        self.receipt.validate(&self.lineage)
    }

    /// Encode only after exact-request validation; adds no persistence or provider wire data.
    /// # Errors
    /// Rejects invalid values, serialization failures or oversized output.
    pub fn encode(
        &self,
        request: &crate::ModelTurnRequest,
        canonical_turn_id: &str,
        model_request_sequence: u64,
    ) -> Result<Vec<u8>, &'static str> {
        self.validate(request, canonical_turn_id, model_request_sequence)?;
        let bytes = serde_json::to_vec(self).map_err(|_| "context observation encoding failed")?;
        if bytes.len() > MAX_CONTEXT_REPRESENTATION_OBSERVATION_BYTES {
            return Err("context observation too large");
        }
        Ok(bytes)
    }

    /// Parse bounded JSON and validate against the independently verified producer request.
    /// # Errors
    /// Rejects oversized/truncated/malformed JSON, unknown fields/versions or identity mismatch.
    pub fn parse(
        bytes: &[u8],
        request: &crate::ModelTurnRequest,
        canonical_turn_id: &str,
        model_request_sequence: u64,
    ) -> Result<Self, &'static str> {
        if bytes.len() > MAX_CONTEXT_REPRESENTATION_OBSERVATION_BYTES {
            return Err("context observation too large");
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| "invalid context observation JSON")?;
        value.validate(request, canonical_turn_id, model_request_sequence)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineage() -> RequestContextLineage {
        RequestContextLineage {
            version: 1,
            request_ordinal: 7,
            omitted: 0,
            entries: vec![ContextLineageEntry {
                source_ordinal: 1,
                operation_ordinal: 2,
                mandatory: true,
                message_index: 0,
                block_index: 0,
            }],
        }
    }

    #[test]
    fn wire_entry_limits_apply_during_deserialization() {
        let mut request = lineage();
        request.entries = (0..MAX_CONTEXT_LINEAGE_ENTRIES)
            .map(|index| {
                let mut entry = request.entries[0].clone();
                entry.source_ordinal = u64::try_from(index).unwrap();
                entry
            })
            .collect();
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(serde_json::from_str::<RequestContextLineage>(&encoded).is_ok());
        request.entries.push(request.entries[0].clone());
        assert!(
            serde_json::from_str::<RequestContextLineage>(
                &serde_json::to_string(&request).unwrap()
            )
            .is_err()
        );
        let mut receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            entries: vec![
                ContextRepresentationEntry {
                    source_ordinal: 1,
                    disposition: ContextRepresentationDisposition::Unknown
                };
                MAX_CONTEXT_LINEAGE_ENTRIES
            ],
            omitted: 0,
        };
        assert!(
            serde_json::from_str::<RequestContextRepresentation>(
                &serde_json::to_string(&receipt).unwrap()
            )
            .is_ok()
        );
        receipt.entries.push(receipt.entries[0].clone());
        assert!(
            serde_json::from_str::<RequestContextRepresentation>(
                &serde_json::to_string(&receipt).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn observation_artifact_requires_exact_request_and_canonical_producer() {
        let mut request: crate::ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000001", "turn_id": "round-1",
            "model_id": "test", "messages": [{"role": "tool", "content": [{"type": "tool_result",
                "result": {"call_id": "call", "output": "private", "is_error": false, "content": []}}]}]
        })).unwrap();
        request.context_lineage = Some(lineage());
        let receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            entries: Vec::new(),
            omitted: 1,
        };
        let observation =
            ContextRepresentationObservation::new(&request, "turn".into(), 9, receipt).unwrap();
        let bytes = observation.encode(&request, "turn", 9).unwrap();
        assert_eq!(
            ContextRepresentationObservation::parse(&bytes, &request, "turn", 9).unwrap(),
            observation
        );
        assert!(ContextRepresentationObservation::parse(&bytes, &request, "other", 9).is_err());
        assert!(ContextRepresentationObservation::parse(&bytes, &request, "turn", 10).is_err());
        assert!(
            ContextRepresentationObservation::parse(&bytes[..bytes.len() - 1], &request, "turn", 9)
                .is_err()
        );
        assert!(
            ContextRepresentationObservation::parse(
                &vec![b' '; MAX_CONTEXT_REPRESENTATION_OBSERVATION_BYTES + 1],
                &request,
                "turn",
                9
            )
            .is_err()
        );
        let mut future = observation.clone();
        future.version = 2;
        assert!(future.validate(&request, "turn", 9).is_err());
        future = observation.clone();
        future.receipt.request_ordinal = 8;
        assert!(future.validate(&request, "turn", 9).is_err());
        request.turn_id = "another-round".into();
        assert!(observation.validate(&request, "turn", 9).is_err());
    }

    #[test]
    fn receipt_rejects_foreign_duplicate_future_and_inconsistent_observations() {
        let lineage = lineage();
        let mut receipt = RequestContextRepresentation {
            version: 1,
            request_ordinal: 7,
            entries: vec![ContextRepresentationEntry {
                source_ordinal: 1,
                disposition: ContextRepresentationDisposition::Unknown,
            }],
            omitted: 0,
        };
        assert!(receipt.validate(&lineage).is_ok());
        receipt.request_ordinal = 8;
        assert!(receipt.validate(&lineage).is_err());
        receipt.request_ordinal = 7;
        receipt.entries.push(receipt.entries[0].clone());
        assert!(receipt.validate(&lineage).is_err());
        receipt.entries.pop();
        receipt.entries[0].source_ordinal = 9;
        assert!(receipt.validate(&lineage).is_err());
        receipt.entries.clear();
        assert!(receipt.validate(&lineage).is_err());
        receipt.omitted = 1;
        assert!(receipt.validate(&lineage).is_ok());
        receipt.version = 2;
        assert!(receipt.validate(&lineage).is_err());
    }

    #[test]
    fn valid_lineage_and_missing_legacy_projection_are_not_delivery_proof() {
        let messages = vec![crate::ModelMessage {
            role: crate::MessageRole::User,
            content: vec![crate::ContentBlock::Text {
                text: "private input".into(),
            }],
        }];
        let mut plan = lineage();
        assert!(plan.validate(&messages).is_ok());
        plan.entries.push(plan.entries[0].clone());
        assert!(plan.validate(&messages).is_err());
        plan.entries.pop();
        plan.version = 2;
        assert!(plan.validate(&messages).is_err());
        let projection: crate::ProviderRequestProjection = serde_json::from_str("{}").unwrap();
        assert!(projection.context_representation.is_none());
        let serialized = serde_json::to_value(lineage()).unwrap();
        let decoded: RequestContextLineage = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded, lineage());
    }

    #[test]
    fn invalid_references_bounds_and_wire_variants_fail_closed() {
        let mut invalid = lineage();
        assert!(invalid.validate(&[]).is_err());
        invalid.entries = vec![invalid.entries[0].clone(); MAX_CONTEXT_LINEAGE_ENTRIES + 1];
        assert!(invalid.validate(&[]).is_err());
        assert!(serde_json::from_str::<ContextRepresentationDisposition>("\"delivered\"").is_err());
        let mut value = serde_json::to_value(super::tests::lineage()).unwrap();
        value["payload"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<RequestContextLineage>(value).is_err());
    }
}
