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
