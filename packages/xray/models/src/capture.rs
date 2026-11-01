//! Explicit capture attachment metadata. Arbitrary diagnostic JSON is never scanned.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{XRAY_SCHEMA_VERSION, XrayEnvelope, XrayEvent, XrayPayloadRef};

/// Independently evolving event capture metadata version, not the envelope version.
pub const XRAY_CAPTURE_SCHEMA_VERSION: u16 = 1;

/// Explicit attachment locations within an event's `data` value.
///
/// Absence on an event means legacy opaque data, not permission to infer references.
/// Present metadata is authoritative: undeclared objects are always ordinary data.
/// Unknown versions, fields and attachment kinds fail deserialization. Consumers must
/// also call [`XrayEvent::payload_references`] before exporting declared payloads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XrayCaptureAttachments {
    /// Independently checked capture metadata version.
    pub schema_version: u16,
    /// Explicit locations, in producer declaration order.
    pub attachments: Vec<XrayCaptureAttachment>,
}

impl XrayCaptureAttachments {
    /// Constructs current-version metadata without inspecting event content.
    #[must_use]
    pub const fn new(attachments: Vec<XrayCaptureAttachment>) -> Self {
        Self {
            schema_version: XRAY_CAPTURE_SCHEMA_VERSION,
            attachments,
        }
    }
}

impl<'de> Deserialize<'de> for XrayCaptureAttachments {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u16,
            attachments: Vec<XrayCaptureAttachment>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != XRAY_CAPTURE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(
                "unsupported X-ray capture metadata version",
            ));
        }
        Ok(Self::new(wire.attachments))
    }
}

/// A typed location, never a filesystem path, URI, or recursive discovery rule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum XrayCaptureAttachment {
    /// An entire [`XrayPayloadRef`] at the exact RFC 6901 pointer relative to `event.data`.
    /// Empty means the data root; URI fragments and invalid `~` escapes are rejected.
    PayloadRef {
        /// JSON pointer into `event.data`, including escaped object keys or array indexes.
        pointer: String,
    },
}

/// Secret-safe attachment validation failure; no diagnostic content is included.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XrayCaptureError {
    /// The outer envelope version is unsupported.
    UnsupportedEnvelopeVersion,
    /// Capture metadata constructed in memory has an unsupported version.
    UnsupportedCaptureVersion,
    /// A pointer is malformed, missing, or declared more than once.
    InvalidLocation,
    /// A declared value is not a supported complete payload reference.
    InvalidReference,
    /// A reference does not belong to the event's origin session.
    ForeignSession,
    /// One opaque payload identity is declared with conflicting metadata.
    ConflictingReference,
}

impl std::fmt::Display for XrayCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedEnvelopeVersion => "unsupported X-ray envelope version",
            Self::UnsupportedCaptureVersion => "unsupported X-ray capture metadata version",
            Self::InvalidLocation => "invalid X-ray attachment location",
            Self::InvalidReference => "invalid X-ray payload reference",
            Self::ForeignSession => "foreign X-ray payload reference",
            Self::ConflictingReference => "conflicting X-ray payload reference",
        })
    }
}

impl std::error::Error for XrayCaptureError {}

impl XrayEvent {
    /// Resolves only explicitly declared payload references, without recursive scanning.
    ///
    /// Legacy events without capture metadata return no references, even when their
    /// data resembles a reference. This does not certify legacy export completeness.
    /// Results preserve declaration order, deduplicating identical payload identities.
    /// Work is proportional to declared locations and their selected values, bounded
    /// by the caller's envelope/attachment input limits, not all nested diagnostic data.
    /// This validates evidence coordinates only; it never authorizes capture or access.
    ///
    /// # Errors
    /// Returns a secret-safe error for unsupported metadata/reference versions,
    /// malformed or duplicate locations, malformed references, foreign sessions, or
    /// conflicting metadata for one payload identity. No partial result is returned.
    pub fn payload_references(&self) -> Result<Vec<XrayPayloadRef>, XrayCaptureError> {
        let Some(capture) = &self.capture else {
            return Ok(Vec::new());
        };
        if capture.schema_version != XRAY_CAPTURE_SCHEMA_VERSION {
            return Err(XrayCaptureError::UnsupportedCaptureVersion);
        }
        let mut locations = BTreeSet::new();
        let mut identities = BTreeMap::new();
        let mut references = Vec::new();
        for attachment in &capture.attachments {
            let XrayCaptureAttachment::PayloadRef { pointer } = attachment;
            if !valid_pointer(pointer) || !locations.insert(pointer) {
                return Err(XrayCaptureError::InvalidLocation);
            }
            let value = self
                .data
                .pointer(pointer)
                .ok_or(XrayCaptureError::InvalidLocation)?;
            let reference: XrayPayloadRef = serde_json::from_value(value.clone())
                .map_err(|_| XrayCaptureError::InvalidReference)?;
            if reference.schema_version != XRAY_SCHEMA_VERSION
                || reference.payload_id.is_empty()
                || reference.content_type.is_empty()
                || reference.sha256.len() != 64
                || !reference
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(XrayCaptureError::InvalidReference);
            }
            if reference.session_id.is_empty()
                || reference.session_id != self.correlation.session_id
            {
                return Err(XrayCaptureError::ForeignSession);
            }
            if let Some(previous) = identities.get(&reference.payload_id) {
                if previous != &reference {
                    return Err(XrayCaptureError::ConflictingReference);
                }
            } else {
                identities.insert(reference.payload_id.clone(), reference.clone());
                references.push(reference);
            }
        }
        Ok(references)
    }
}

impl XrayEnvelope {
    /// Checks the envelope version and resolves explicitly declared capture references.
    ///
    /// # Errors
    /// Rejects unsupported envelope versions and all attachment errors documented by
    /// [`XrayEvent::payload_references`]. Observation authorization remains caller-owned.
    pub fn payload_references(&self) -> Result<Vec<XrayPayloadRef>, XrayCaptureError> {
        if self.schema_version != XRAY_SCHEMA_VERSION {
            return Err(XrayCaptureError::UnsupportedEnvelopeVersion);
        }
        self.event.payload_references()
    }
}

fn valid_pointer(pointer: &str) -> bool {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return false;
    }
    let mut bytes = pointer.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests;
