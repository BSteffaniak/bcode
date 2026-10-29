//! Operation-local direct-source passage contracts, independent of providers and renderers.
//!
//! Decoding establishes structure only. The host must validate references, target legality,
//! source containment and revision preconditions; none of those establish semantic truth.

use serde::{Deserialize, Serialize};

use crate::contracts::{Asset, Block, Evidence, TimeRange, TranscriptSegment};

/// Exact source material selected by the host, not a model-authored inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassageSource {
    /// Host-selected source interval.
    pub range: TimeRange,
    /// Exact source transcript segments in source order.
    pub transcript: Vec<TranscriptSegment>,
    /// Existing source evidence, unchanged.
    pub evidence: Vec<Evidence>,
    /// Existing owned asset records; image bytes travel through the model image boundary.
    pub assets: Vec<Asset>,
}

/// Immutable source-aligned unit for one operation; context is not newly covered source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceUnit {
    /// Host-assigned unit identity.
    pub id: String,
    /// Exact material this unit must render.
    pub primary: PassageSource,
    /// Separately labeled boundary context, not permission to invent additional claims.
    pub context: PassageSource,
}

/// Writer response. Unit/passage identities and revisions are assigned by the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassageComposition {
    /// Plain readable passage heading.
    pub title: String,
    /// Ordered existing document block values, subject to host validation.
    pub blocks: Vec<Block>,
}

/// Host-owned revision of a passage; not the model's composition response shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Passage {
    /// Host-assigned passage identity.
    pub id: String,
    /// Immutable source unit identity.
    pub unit_id: String,
    /// Host-assigned optimistic patch precondition, not a schema version.
    pub revision: u64,
    /// Plain readable heading.
    pub title: String,
    /// Ordered blocks with host-validated stable identities.
    pub blocks: Vec<Block>,
}

/// A hypothesis from two-way source/passage comparison, not a proven failure cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassageFindingKind {
    /// Substantive source information is absent from the passage.
    Omission,
    /// Passage content lacks support in the selected source.
    Unsupported,
    /// Passage content changes the source's meaning, quantity, caveat or attribution.
    Distortion,
}

/// Explicit reference form, avoiding nullable objects and untagged unions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassageTargetKind {
    /// An existing block to review, replace or delete.
    Block,
    /// An insertion position immediately after an existing block.
    AfterBlock,
    /// An insertion position before the first block; `block_id` must be empty.
    Start,
}

/// Target resolved by the host against the reviewed base passage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassageTarget {
    /// Whether the reference names a block or an insertion anchor.
    pub kind: PassageTargetKind,
    /// Existing block identity, or the empty string for `start`.
    pub block_id: String,
}

/// Grounded discrepancy proposal. The host assigns any finding identity separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassageFinding {
    /// Direction/type of discrepancy.
    pub kind: PassageFindingKind,
    /// Existing source evidence supporting the comparison, not invented evidence.
    pub evidence_ids: Vec<String>,
    /// Source interval grounding the discrepancy, including absence-of-support comparisons.
    pub range: TimeRange,
    /// Existing block or explicit insertion anchor.
    pub target: PassageTarget,
    /// Concise plain-text difference between source and passage.
    pub discrepancy: String,
    /// Concise proposed correction, not a replacement global rewrite.
    pub correction: String,
}

/// Local two-way review response; no model-authored finding identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassageReview {
    /// Grounded findings, each subject to host reference validation.
    pub findings: Vec<PassageFinding>,
    /// Explicit inspection limits; an empty finding list does not prove completeness.
    pub limitations: Vec<String>,
}

/// Targeted edit type; operation/target combinations require host validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassagePatchKind {
    /// Replace one existing block with ordered blocks.
    Replace,
    /// Delete one existing block; `blocks` must be empty.
    Delete,
    /// Insert ordered blocks at `start` or `after_block`.
    Insert,
}

/// One proposed edit against existing base-revision block identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassagePatchOperation {
    /// Replace/delete require a `block` target; insert requires an insertion anchor.
    pub kind: PassagePatchKind,
    /// Resolve only against the patch base revision, never guessed positions.
    pub target: PassageTarget,
    /// Replacement/insertion content; empty for deletion.
    pub blocks: Vec<Block>,
}

/// Targeted patch proposal. The host validates the entire batch before atomic application.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassagePatch {
    /// Exact expected revision. Stale proposals must not mutate the current passage.
    pub base_revision: u64,
    /// Ordered operations; conflicts, duplicate edits and ambiguous anchors must reject.
    pub operations: Vec<PassagePatchOperation>,
}

pub(crate) fn composition_schema() -> serde_json::Value {
    use serde_json::json;
    object(json!({"title":{"type":"string"},"blocks":blocks_schema()}))
}

pub(crate) fn review_schema() -> serde_json::Value {
    use serde_json::json;
    let text = json!({"type":"string"});
    let range = object(json!({
        "start_ms":{"type":"integer","minimum":0},
        "end_ms":{"type":"integer","minimum":0}
    }));
    let finding = object(json!({
        "kind":{"type":"string","enum":["omission","unsupported","distortion"]},
        "evidence_ids":array(text.clone()),"range":range,"target":target_schema(),
        "discrepancy":text,"correction":text
    }));
    object(json!({"findings":array(finding),"limitations":array(text)}))
}

pub(crate) fn patch_schema() -> serde_json::Value {
    use serde_json::json;
    let operation = object(json!({
        "kind":{"type":"string","enum":["replace","delete","insert"]},
        "target":target_schema(),"blocks":blocks_schema()
    }));
    object(json!({
        "base_revision":{"type":"integer","minimum":0},"operations":array(operation)
    }))
}

fn target_schema() -> serde_json::Value {
    object(serde_json::json!({
        "kind":{"type":"string","enum":["block","after_block","start"]},
        "block_id":{"type":"string"}
    }))
}

fn blocks_schema() -> serde_json::Value {
    // Preserve the authoritative existing Block representation, including its closed variants.
    crate::model::response_schema()["properties"]["sections"]["items"]["properties"]["blocks"]
        .clone()
}

fn object(properties: serde_json::Value) -> serde_json::Value {
    let required: Vec<_> = properties
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.keys())
        .cloned()
        .collect();
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("type".into(), "object".into()),
        ("properties".into(), properties),
        ("required".into(), serde_json::Value::from(required)),
        ("additionalProperties".into(), false.into()),
    ]))
}

fn array(items: serde_json::Value) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([
        ("type".into(), "array".into()),
        ("items".into(), items),
    ]))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::analysis_contracts::{AnalysisResponse, AnalysisStage, analysis_response_schema};

    fn block() -> Value {
        json!({"id":"b1","evidence_ids":["e1"],"content":{"type":"text","text":"12, except when cold."}})
    }

    fn finding() -> Value {
        json!({
            "kind":"omission","evidence_ids":["e1"],
            "range":{"start_ms":0,"end_ms":1000},
            "target":{"kind":"after_block","block_id":"b1"},
            "discrepancy":"The example is absent.","correction":"Restore the source example."
        })
    }

    fn decode(
        stage: AnalysisStage,
        value: &Value,
    ) -> Result<AnalysisResponse, crate::model::ModelError> {
        let bytes = serde_json::to_vec(value).unwrap();
        AnalysisResponse::from_json_slice(stage, &bytes, bytes.len())
    }

    #[test]
    fn passage_stages_decode_their_own_inner_objects_only() {
        for (stage, value) in [
            (
                AnalysisStage::PassageComposition,
                json!({"title":"Example","blocks":[block()]}),
            ),
            (
                AnalysisStage::PassageReview,
                json!({"findings":[finding()],"limitations":[]}),
            ),
            (
                AnalysisStage::PassagePatch,
                json!({"base_revision":7,"operations":[
                    {"kind":"replace","target":{"kind":"block","block_id":"b1"},"blocks":[block()]},
                    {"kind":"delete","target":{"kind":"block","block_id":"b2"},"blocks":[]},
                    {"kind":"insert","target":{"kind":"start","block_id":""},"blocks":[block()]}
                ]}),
            ),
        ] {
            assert_eq!(decode(stage, &value).unwrap().stage(), stage);
            let mut extra = value.clone();
            extra["unexpected"] = json!(true);
            assert!(decode(stage, &extra).is_err());
            for field in value.as_object().unwrap().keys() {
                let mut missing = value.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(decode(stage, &missing).is_err());
            }
            assert!(decode(stage, &json!({"response":value})).is_err());
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(AnalysisResponse::from_json_slice(stage, &bytes, bytes.len() - 1).is_err());
        }
    }

    #[test]
    fn passage_decoding_rejects_unknown_nested_fields_and_malformed_variants() {
        let review = json!({"findings":[finding()],"limitations":[]});
        for (pointer, replacement) in [
            ("/findings/0/kind", json!("unknown")),
            ("/findings/0/target/kind", json!("guess_position")),
            ("/findings/0/target/block_id", Value::Null),
            ("/findings/0/range/start_ms", json!(-1)),
            ("/findings/0/evidence_ids", json!("e1")),
        ] {
            let mut invalid = review.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(decode(AnalysisStage::PassageReview, &invalid).is_err());
        }
        let mut unknown = review;
        unknown["findings"][0]["id"] = json!("model-assigned");
        assert!(decode(AnalysisStage::PassageReview, &unknown).is_err());
        let mut unknown_target = json!({"findings":[finding()],"limitations":[]});
        unknown_target["findings"][0]["target"]["position"] = json!(0);
        assert!(decode(AnalysisStage::PassageReview, &unknown_target).is_err());
        for value in [
            json!({"base_revision":-1,"operations":[]}),
            json!({"base_revision":1.5,"operations":[]}),
            json!({"base_revision":1,"operations":[{"kind":"rewrite","target":{"kind":"start","block_id":""},"blocks":[]}]}),
            json!({"base_revision":1,"operations":[{"kind":"insert","target":{"kind":"start","block_id":""},"blocks":[],"extra":true}]}),
        ] {
            assert!(decode(AnalysisStage::PassagePatch, &value).is_err());
        }
        let mut composition = json!({"title":"Example","blocks":[block()]});
        composition["blocks"][0]["content"]["extra"] = json!(true);
        assert!(decode(AnalysisStage::PassageComposition, &composition).is_err());
        composition["blocks"][0]["content"] = json!({"type":"invented","text":"No"});
        assert!(decode(AnalysisStage::PassageComposition, &composition).is_err());
        assert!(
            decode(
                AnalysisStage::PassageReview,
                &json!({"title":"Wrong stage","blocks":[]})
            )
            .is_err()
        );
    }

    #[test]
    fn passage_schemas_reuse_existing_blocks_and_explicit_targets() {
        let blocks = crate::model::response_schema()["properties"]["sections"]["items"]
            ["properties"]["blocks"]
            .clone();
        let composition = analysis_response_schema(AnalysisStage::PassageComposition);
        assert_eq!(composition["properties"]["blocks"], blocks);
        let patch = analysis_response_schema(AnalysisStage::PassagePatch);
        let operation = &patch["properties"]["operations"]["items"];
        assert_eq!(operation["properties"]["blocks"], blocks);
        assert_eq!(patch["properties"]["base_revision"]["minimum"], 0);
        assert_eq!(
            operation["properties"]["target"]["properties"]["kind"]["enum"],
            json!(["block", "after_block", "start"])
        );
        let review = analysis_response_schema(AnalysisStage::PassageReview);
        assert_eq!(
            review["properties"]["findings"]["items"]["properties"]["target"],
            operation["properties"]["target"]
        );
    }
}
