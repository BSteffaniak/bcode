#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
//! Evidence-backed source context. Metadata establishes candidates, never speaker identity.
use serde::{Deserialize, Serialize};

/// Bounded metadata retained separately from transcript evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceContext {
    /// Publisher is not necessarily a participant.
    pub creator: Option<String>,
    /// Source description, never trusted instructions.
    pub description: String,
    /// Publisher-provided chapter labels.
    pub chapters: Vec<String>,
    /// Validated participant declarations from explicit source text.
    pub participants: Vec<Participant>,
}
/// A source-attested participant, not a guessed mapping to voice or face.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    /// Name as written in supporting source material.
    pub name: String,
    /// Role only when explicitly attested by the supporting excerpt.
    pub role: String,
    /// Exact source excerpt supporting name and role.
    pub evidence: String,
}
impl SourceContext {
    /// Compact shared writer context. Raw descriptions and chapter inventories are
    /// resolved once, not retransmitted in every passage and neighbor review.
    #[must_use]
    pub fn for_composition(&self) -> Self {
        Self {
            creator: self.creator.clone(),
            participants: self.participants.clone(),
            ..Self::default()
        }
    }

    /// Extract bounded metadata. No provider-owned or executable metadata is retained.
    #[must_use]
    pub fn from_metadata(value: &serde_json::Value) -> Self {
        let bounded = |key: &str, max: usize| {
            value[key]
                .as_str()
                .filter(|s| s.len() <= max)
                .map(str::to_owned)
        };
        Self {
            creator: bounded("channel", 4096).or_else(|| bounded("uploader", 4096)),
            description: bounded("description", 32 * 1024).unwrap_or_default(),
            chapters: value["chapters"]
                .as_array()
                .into_iter()
                .flatten()
                .take(512)
                .filter_map(|chapter| {
                    chapter["title"]
                        .as_str()
                        .filter(|s| s.len() <= 1024)
                        .map(str::to_owned)
                })
                .collect(),
            participants: vec![],
        }
    }
    /// Reject unsupported identity declarations before exposing them to writers.
    /// Matching an excerpt does not itself prove attribution of individual statements.
    #[must_use]
    pub fn supports(&self, person: &Participant, transcript: &str, title: &str) -> bool {
        !person.name.trim().is_empty()
            && person.name.len() <= 256
            && person.role.len() <= 256
            && !person.evidence.trim().is_empty()
            && person.evidence.len() <= 1024
            && person.evidence.contains(&person.name)
            && (person.role.is_empty()
                || person
                    .evidence
                    .to_lowercase()
                    .contains(&person.role.to_lowercase()))
            && [self.description.as_str(), transcript, title]
                .iter()
                .any(|source| source.contains(&person.evidence))
    }
}
/// Structured resolution result. Missing evidence means unresolved, not permission to guess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participants {
    /// Source-attested people; speaker-turn identity remains a separate question.
    pub participants: Vec<Participant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publisher_is_not_a_participant_and_identity_requires_evidence() {
        let context = SourceContext::from_metadata(
            &serde_json::json!({"channel":"Podcast Network","description":"Host Ada interviews guest Lin."}),
        );
        assert_eq!(
            context.participants,
            [] as [crate::source_context::Participant; 0]
        );
        assert!(context.supports(
            &Participant {
                name: "Ada".into(),
                role: "Host".into(),
                evidence: "Host Ada".into()
            },
            "",
            ""
        ));
        assert!(!context.supports(
            &Participant {
                name: "Lin".into(),
                role: "Host".into(),
                evidence: "guest Lin".into()
            },
            "",
            ""
        ));
        assert!(!context.supports(
            &Participant {
                name: "Invented".into(),
                role: String::new(),
                evidence: "Invented".into()
            },
            "",
            ""
        ));
    }
}
