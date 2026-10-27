//! Renderer-neutral recap documents and validation. Text is plain text, never trusted HTML.
//!
//! Deserialization alone is not validation: callers must use [`RecapDocument::from_json_slice`]
//! or [`RecapDocument::validate`] before consuming external documents.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The only supported document representation. Unknown versions are never downgraded.
pub const DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// A recap operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// Contract and persistence failures. Failed saves do not imply a successful commit unless
/// [`Error::DurabilityUncertain`] is returned.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The representation must be handled by a newer or explicitly migrating reader.
    #[error("unsupported recap schema version {0}")]
    UnsupportedVersion(u32),
    /// A document violates a structural or reference constraint.
    #[error("invalid recap document: {0}")]
    InvalidDocument(String),
    /// An asset path is not a portable, owned relative path.
    #[error("invalid recap asset path")]
    InvalidAssetPath,
    /// The stored revision does not match the caller's precondition.
    #[error("recap revision conflict (expected {expected:?}, actual {actual:?})")]
    RevisionConflict {
        /// Caller's precondition; `None` means absent.
        expected: Option<u64>,
        /// Observed revision; `None` means absent.
        actual: Option<u64>,
    },
    /// Another writer holds the lock, or an interrupted writer needs explicit recovery.
    #[error("recap is locked; verify any prior writer has ended before removing its lock")]
    Locked,
    /// A configured input or output byte budget was exceeded.
    #[error("recap byte limit exceeded ({limit} bytes)")]
    TooLarge {
        /// Configured maximum bytes.
        limit: u64,
    },
    /// A storage entry is a symlink, non-file, or outside its owned root.
    #[error("recap storage path is not an owned regular file")]
    UnsafeStoragePath,
    /// Filesystem access failed. Paths and raw OS messages are not public diagnostics.
    #[error("recap storage I/O failed")]
    Io(#[from] std::io::Error),
    /// The JSON representation is malformed.
    #[error("invalid recap JSON")]
    Json(#[from] serde_json::Error),
    /// Rename committed, but directory synchronization failed. Reload before retrying.
    #[error("recap committed but durable synchronization is uncertain; reload before retrying")]
    DurabilityUncertain(#[source] std::io::Error),
}

/// Original video metadata. URLs are descriptive provenance, not executable markup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Stable source identity.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Original location, when available; never required for offline rendering.
    pub url: Option<String>,
    /// Total video length in milliseconds.
    pub duration_ms: u64,
}

/// A nonempty half-open source interval `[start_ms, end_ms)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeRange {
    /// Inclusive start offset.
    pub start_ms: u64,
    /// Exclusive end offset.
    pub end_ms: u64,
}

impl TimeRange {
    /// Validate a nonempty interval contained within the source.
    ///
    /// # Errors
    /// Returns [`Error::InvalidDocument`] for reversed, empty, or out-of-source ranges.
    pub fn validate(self, duration_ms: u64) -> Result<()> {
        if self.start_ms >= self.end_ms || self.end_ms > duration_ms {
            return invalid("time range must be nonempty and within the source");
        }
        Ok(())
    }

    const fn contains(self, other: Self) -> bool {
        self.start_ms <= other.start_ms && other.end_ms <= self.end_ms
    }
}

/// A source-aligned transcript excerpt; absence of a transcript is supported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSegment {
    /// Stable segment identity.
    pub id: String,
    /// Source interval covered by the words.
    pub range: TimeRange,
    /// Plain transcript text.
    pub text: String,
}

/// Source grounding for a claim or visual selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// Stable evidence identity.
    pub id: String,
    /// Source interval supporting this evidence.
    pub range: TimeRange,
    /// Optional transcript support; each referenced segment must lie inside `range`.
    pub transcript_segment_ids: Vec<String>,
}

/// An owned, portable relative asset path. It is not a URL or HTML fragment.
/// Filesystem access must additionally verify confinement and reject symlinks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AssetPath(String);

impl AssetPath {
    /// Parse a slash-separated path with ASCII filename components.
    ///
    /// # Errors
    /// Rejects absolute paths, traversal, URLs, encoded separators, backslashes,
    /// control characters, dot components, and nonportable filename characters.
    pub fn new(path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        if path.is_empty()
            || path.len() > 1024
            || path.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.ends_with('.')
                    || reserved_filename(part)
                    || part.len() > 255
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        {
            return Err(Error::InvalidAssetPath);
        }
        Ok(Self(path))
    }

    /// Borrow the portable relative path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AssetPath {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<AssetPath> for String {
    fn from(value: AssetPath) -> Self {
        value.0
    }
}

/// Owned media extracted from the document's source. Only passive raster images
/// and video formats are supported; SVG and HTML are deliberately excluded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// Stable asset identity.
    pub id: String,
    /// Relative to this document's owned assets directory.
    pub path: AssetPath,
    /// Exact media type: image/png, image/jpeg, image/webp, video/mp4, or video/webm.
    pub mime_type: String,
    /// Source interval represented by this asset, including still-frame provenance.
    pub range: TimeRange,
}

/// Desired recap detail without requiring a conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    /// A short visual overview.
    Brief,
    /// A useful visual recap, the noninteractive default.
    #[default]
    Standard,
    /// A more comprehensive recap.
    Detailed,
}

/// Optional generation/refinement preferences. This contains no provider wire data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationIntent {
    /// Optional plain-language focus; `None` requests a general recap.
    pub focus: Option<String>,
    /// Requested level of detail.
    pub depth: Depth,
}

/// A stable semantic section; ordering is presentation order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// Stable section identity retained during refinement where possible.
    pub id: String,
    /// Plain section heading.
    pub title: String,
    /// Ordered content blocks.
    pub blocks: Vec<Block>,
}

/// A stable semantic content unit with explicit source grounding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    /// Stable identity, unique across all sections.
    pub id: String,
    /// Evidence supporting the content; may be empty for editorial headings/context.
    pub evidence_ids: Vec<String>,
    /// Renderer-neutral content.
    pub content: BlockContent,
}

/// Plain semantic content. Renderers must escape all text when generating markup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockContent {
    /// A paragraph.
    Text {
        /// Plain text.
        text: String,
    },
    /// An extracted still image.
    Image {
        /// Image asset reference.
        asset_id: String,
        /// Plain description used as accessible alternative text.
        caption: String,
    },
    /// A self-contained video excerpt.
    Clip {
        /// Video asset reference.
        asset_id: String,
        /// Plain clip description.
        caption: String,
    },
    /// Two or more labeled images, e.g. before/after or alternate techniques.
    Comparison {
        /// Ordered image selections.
        items: Vec<ComparisonItem>,
    },
}

/// An image within a semantic comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonItem {
    /// Plain label describing the compared state.
    pub label: String,
    /// Image asset reference.
    pub asset_id: String,
}

/// Complete portable recap. Identity is stable; revisions start at 1 and advance
/// on every persisted edit. Schema version describes representation, not revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecapDocument {
    /// Independently evolving document format version.
    pub schema_version: u32,
    /// Stable document identity, also used for owned storage names.
    pub id: String,
    /// Monotonically increasing content revision, starting at 1.
    pub revision: u64,
    /// Plain recap title.
    pub title: String,
    /// The original video.
    pub source: Source,
    /// Preferences used for this revision.
    pub intent: GenerationIntent,
    /// Available transcript excerpts.
    pub transcript: Vec<TranscriptSegment>,
    /// Grounding references.
    pub evidence: Vec<Evidence>,
    /// Owned offline media references.
    pub assets: Vec<Asset>,
    /// Ordered semantic sections.
    pub sections: Vec<Section>,
}

impl RecapDocument {
    /// Decode a document after checking its compatibility header first.
    /// The caller must bound input bytes before invoking this method.
    ///
    /// # Errors
    /// Returns [`Error::UnsupportedVersion`] before interpreting an unknown
    /// payload, [`Error::Json`] for malformed JSON, or validation errors.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }
        let header: Header = serde_json::from_slice(bytes)?;
        check_version(header.schema_version)?;
        let document: Self = serde_json::from_slice(bytes)?;
        document.validate()?;
        Ok(document)
    }

    /// Validate identity, time provenance, media types, uniqueness, and references.
    ///
    /// # Errors
    /// Returns [`Error::UnsupportedVersion`] or [`Error::InvalidDocument`] for
    /// unsupported or inconsistent data. No filesystem access is performed.
    pub fn validate(&self) -> Result<()> {
        check_version(self.schema_version)?;
        validate_id(&self.id)?;
        validate_id(&self.source.id)?;
        if self.revision == 0 || self.source.duration_ms == 0 {
            return invalid("revision and source duration must be positive");
        }
        nonempty(&self.title)?;
        nonempty(&self.source.title)?;
        if let Some(focus) = &self.intent.focus {
            nonempty(focus)?;
        }
        let transcript_ids = unique_ids(self.transcript.iter().map(|segment| segment.id.as_str()))?;
        let evidence_ids = unique_ids(self.evidence.iter().map(|item| item.id.as_str()))?;
        unique_ids(self.assets.iter().map(|asset| asset.id.as_str()))?;
        unique_ids(self.sections.iter().map(|section| section.id.as_str()))?;
        unique_ids(
            self.sections
                .iter()
                .flat_map(|section| section.blocks.iter().map(|block| block.id.as_str())),
        )?;
        let mut paths = BTreeSet::new();
        for segment in &self.transcript {
            segment.range.validate(self.source.duration_ms)?;
            nonempty(&segment.text)?;
        }
        for evidence in &self.evidence {
            evidence.range.validate(self.source.duration_ms)?;
            references(&evidence.transcript_segment_ids, &transcript_ids)?;
            for id in &evidence.transcript_segment_ids {
                let segment = self
                    .transcript
                    .iter()
                    .find(|segment| segment.id == *id)
                    .ok_or_else(|| Error::InvalidDocument("missing transcript reference".into()))?;
                if !evidence.range.contains(segment.range) {
                    return invalid("transcript support lies outside evidence range");
                }
            }
        }
        for asset in &self.assets {
            asset.range.validate(self.source.duration_ms)?;
            if !paths.insert(&asset.path) {
                return invalid("duplicate asset path");
            }
            if !matches!(
                asset.mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp" | "video/mp4" | "video/webm"
            ) {
                return invalid("unsupported asset MIME type");
            }
        }
        for section in &self.sections {
            nonempty(&section.title)?;
            for block in &section.blocks {
                references(&block.evidence_ids, &evidence_ids)?;
                match &block.content {
                    BlockContent::Text { text } => nonempty(text)?,
                    BlockContent::Image { asset_id, caption } => {
                        self.validate_media(asset_id, "image/")?;
                        nonempty(caption)?;
                    }
                    BlockContent::Clip { asset_id, caption } => {
                        self.validate_media(asset_id, "video/")?;
                        nonempty(caption)?;
                    }
                    BlockContent::Comparison { items } => {
                        if items.len() < 2 {
                            return invalid("comparison requires at least two images");
                        }
                        for item in items {
                            nonempty(&item.label)?;
                            self.validate_media(&item.asset_id, "image/")?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_media(&self, id: &str, prefix: &str) -> Result<()> {
        if self
            .assets
            .iter()
            .any(|asset| asset.id == id && asset.mime_type.starts_with(prefix))
        {
            Ok(())
        } else {
            invalid("missing asset reference or incompatible media type")
        }
    }
}

const fn check_version(version: u32) -> Result<()> {
    if version != DOCUMENT_SCHEMA_VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    Ok(())
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || reserved_filename(id)
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return invalid("IDs must be 1-128 ASCII letters, digits, hyphens, or underscores");
    }
    Ok(())
}

// Windows device basenames remain reserved even when followed by an extension.
fn reserved_filename(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<BTreeSet<&'a str>> {
    let mut seen = BTreeSet::new();
    for id in ids {
        validate_id(id)?;
        if !seen.insert(id) {
            return invalid("duplicate stable ID");
        }
    }
    Ok(seen)
}

fn references(ids: &[String], available: &BTreeSet<&str>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !available.contains(id.as_str()) || !seen.insert(id) {
            return invalid("missing or duplicate reference");
        }
    }
    Ok(())
}

fn nonempty(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return invalid("required text is empty");
    }
    Ok(())
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::InvalidDocument(message.into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn document() -> RecapDocument {
        RecapDocument {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            id: "recap-1".into(),
            revision: 1,
            title: "A visual recap".into(),
            source: Source {
                id: "video-1".into(),
                title: "Original".into(),
                url: None,
                duration_ms: 10_000,
            },
            intent: GenerationIntent::default(),
            transcript: vec![TranscriptSegment {
                id: "words".into(),
                range: TimeRange {
                    start_ms: 0,
                    end_ms: 1000,
                },
                text: "Hello".into(),
            }],
            evidence: vec![Evidence {
                id: "evidence".into(),
                range: TimeRange {
                    start_ms: 0,
                    end_ms: 2000,
                },
                transcript_segment_ids: vec!["words".into()],
            }],
            assets: vec![Asset {
                id: "frame".into(),
                path: AssetPath::new("frames/first.png").unwrap(),
                mime_type: "image/png".into(),
                range: TimeRange {
                    start_ms: 500,
                    end_ms: 501,
                },
            }],
            sections: vec![Section {
                id: "intro".into(),
                title: "Introduction".into(),
                blocks: vec![Block {
                    id: "opening".into(),
                    evidence_ids: vec!["evidence".into()],
                    content: BlockContent::Image {
                        asset_id: "frame".into(),
                        caption: "Opening frame".into(),
                    },
                }],
            }],
        }
    }

    #[test]
    fn valid_document_round_trips() {
        let original = document();
        let bytes = serde_json::to_vec(&original).unwrap();
        assert_eq!(RecapDocument::from_json_slice(&bytes).unwrap(), original);
        assert_eq!(GenerationIntent::default().depth, Depth::Standard);
    }

    #[test]
    fn future_version_is_rejected_before_payload_interpretation() {
        assert!(matches!(
            RecapDocument::from_json_slice(br#"{"schema_version":2,"future_payload":true}"#),
            Err(Error::UnsupportedVersion(2))
        ));
        let mut value = document();
        value.schema_version = 0;
        assert!(matches!(
            value.validate(),
            Err(Error::UnsupportedVersion(0))
        ));
    }

    #[test]
    fn paths_reject_untrusted_and_nonportable_forms() {
        for path in [
            "",
            "/tmp/image.png",
            "../image.png",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "a\\b",
            "C:/a",
            "https://x/a",
            "a%2fb",
            "a\0b",
            "a\nb",
            "frames/NUL.png",
            "COM1",
            "frames/image.",
        ] {
            assert!(AssetPath::new(path).is_err(), "{path:?}");
            assert!(
                serde_json::from_str::<AssetPath>(&serde_json::to_string(path).unwrap()).is_err()
            );
        }
        assert!(AssetPath::new("frames/frame-001.webp").is_ok());
    }

    #[test]
    fn invalid_ranges_and_references_fail() {
        for range in [
            TimeRange {
                start_ms: 4,
                end_ms: 4,
            },
            TimeRange {
                start_ms: 9,
                end_ms: 8,
            },
            TimeRange {
                start_ms: 0,
                end_ms: 10_001,
            },
        ] {
            let mut value = document();
            value.assets[0].range = range;
            assert!(value.validate().is_err());
        }
        let mut value = document();
        value.sections[0].blocks[0]
            .evidence_ids
            .push("missing".into());
        assert!(value.validate().is_err());
        value = document();
        value.evidence[0].range.start_ms = 1;
        assert!(value.validate().is_err());
        value = document();
        value.assets[0].mime_type = "video/mp4".into();
        assert!(value.validate().is_err());
        value = document();
        value.sections.push(value.sections[0].clone());
        assert!(value.validate().is_err());
        value = document();
        value.assets[0].mime_type = "image/svg+xml".into();
        assert!(value.validate().is_err());
    }
}
