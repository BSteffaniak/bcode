//! Write operations applied to versions under each grant.

use super::*;
use bcode_code_review_models::{
    CitationCheckStatus, CitationRelation, GuideOrigin, REVIEW_GUIDE_SCHEMA_VERSION,
    ReviewLineInterval, ReviewLineSide, RevisionRef, Verification,
};
use serde_json::json;

fn version() -> GuideVersion {
    GuideVersion {
        guide_id: "g".to_owned(),
        version: 1,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads: BTreeMap::new(),
        based_on: None,
        origin: GuideOrigin::default(),
        status: GuideStatus::Generating,
        generation: None,
        outline: Vec::new(),
        citations: BTreeMap::new(),
        retired_sections: Vec::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn ops(value: serde_json::Value) -> Vec<GuideOp> {
    serde_json::from_value(value).expect("ops")
}

/// A verified check of each request, as the citation checks would answer.
fn checks(ops: &[GuideOp]) -> Vec<CitationCheck> {
    batch_citations(ops)
        .into_iter()
        .map(|request| CitationCheck {
            citation: Citation {
                id: String::new(),
                source_id: request.source.clone(),
                path: request.path.clone(),
                side: request.side,
                lines: ReviewLineInterval {
                    start: request.start_line,
                    end: request.end_line,
                },
                revision: RevisionRef::commit("0".repeat(40)),
                blob_oid: None,
                quote_hash: "h".to_owned(),
                excerpt: request.quote.unwrap_or_default(),
                relation: CitationRelation::Changed,
                verification: Verification::Verified {
                    at_ms: 1,
                    adjusted_from: None,
                },
            },
            status: CitationCheckStatus::Verified,
            problems: Vec::new(),
            warnings: Vec::new(),
        })
        .collect()
}

fn run(
    version: &mut GuideVersion,
    mode: &GrantMode,
    value: serde_json::Value,
) -> Result<ApplyOutcome, Vec<OpError>> {
    let ops = ops(value);
    let checks = checks(&ops);
    apply(
        version,
        &ops,
        &checks,
        &Writer {
            mode,
            session_id: Some("session"),
            now_ms: 5,
        },
    )
}

/// A version with a written outline: s1 purpose, s3 reading order with step s3.1.
fn written() -> GuideVersion {
    let mut version = version();
    run(
        &mut version,
        &GrantMode::Generate,
        json!([
            {"op": "set_outline", "sections": [
                {"id": "s1", "kind": "purpose", "title": "Purpose"},
                {"id": "s3", "kind": "reading_order", "title": "Reading order", "children": [
                    {"id": "s3.1", "kind": "step", "title": "RetryBudget"}
                ]},
                {"id": "s4", "kind": "risks", "title": "Risks"}
            ]},
            {"op": "put_section", "id": "s1", "body": "Adds a retry budget."},
            {"op": "put_section", "id": "s3.1", "body": "A bucket [[1]] refilled [[2]].",
             "citations": [
                {"path": "net/budget.rs", "start_line": 12, "end_line": 40, "quote": "struct RetryBudget"},
                {"path": "net/budget.rs", "start_line": 50, "end_line": 52, "quote": "fn refill"}
             ],
             "findings": [{"kind": "risk", "severity": "warning", "text": "Refill races.", "citations": [2]}]}
        ]),
    )
    .expect("generate");
    version
}

#[test]
fn an_outline_is_planned_then_sections_are_written_with_their_citations() {
    let version = written();
    let step = version.section("s3.1").expect("step");
    assert_eq!(step.state, SectionState::Written);
    assert_eq!(step.citations, ["c1", "c2"]);
    assert_eq!(
        step.body, "A bucket [[c1]] refilled [[c2]].",
        "numbered tokens become ids"
    );
    assert_eq!(step.findings[0].citations, ["c2"]);
    assert_eq!(step.findings[0].id, "f1");
    assert_eq!(version.citations["c1"].id, "c1");
    assert_eq!(
        version.section("s4").expect("risks").state,
        SectionState::Planned
    );
    assert_eq!(version.updated_at_ms, 5);
}

#[test]
fn a_refused_operation_writes_nothing_and_says_which_it_was() {
    let mut version = written();
    let before = version.clone();
    let errors = run(
        &mut version,
        &GrantMode::Generate,
        json!([
            {"op": "put_section", "id": "s1", "body": "Changed."},
            {"op": "put_section", "id": "s9", "body": "No kind or title."},
            {"op": "remove_section", "id": "s3.1"}
        ]),
    )
    .expect_err("refused");
    assert_eq!(version, before, "nothing was written");
    let codes: Vec<(usize, &str)> = errors
        .iter()
        .map(|error| (error.op_index, error.code))
        .collect();
    assert_eq!(codes, [(1, INVALID_OP), (2, OUTSIDE_GRANT)]);
}

#[test]
fn an_expand_grant_adds_only_under_its_section() {
    let mut version = written();
    let mode = GrantMode::Expand {
        section_id: "s3.1".to_owned(),
    };
    let outcome = run(
        &mut version,
        &mode,
        json!([{"op": "add_expansion", "under": "s3.1", "title": "Why a token bucket",
                "body": "Bursts [[1]].",
                "citations": [{"path": "net/budget.rs", "start_line": 1, "end_line": 2, "quote": "use"}]}]),
    )
    .expect("expand");
    assert_eq!(outcome.sections_written, ["s3.1.x1"]);
    let expansion = version.section("s3.1.x1").expect("expansion");
    assert!(matches!(
        expansion.state,
        SectionState::Provisional {
            origin: ProvisionalOrigin::Expand,
            ..
        }
    ));
    assert_eq!(expansion.kind, SectionKind::Expansion);
    let others = version
        .outline
        .iter()
        .map(|section| section.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(others, ["s1", "s3", "s4"]);

    for refused in [
        json!([{"op": "put_section", "id": "s1", "body": "Rewritten."}]),
        json!([{"op": "add_expansion", "under": "s1", "title": "Elsewhere"}]),
        json!([{"op": "promote", "section": "s3.1.x1"}]),
        json!([{"op": "set_status", "status": "complete"}]),
    ] {
        let errors = run(&mut version, &mode, refused.clone()).expect_err("outside");
        assert_eq!(errors[0].code, OUTSIDE_GRANT, "{refused}");
        assert!(errors[0].message.contains("s3.1"), "{}", errors[0].message);
    }
    run(
        &mut version,
        &mode,
        json!([{"op": "discard", "section": "s3.1.x1"}]),
    )
    .expect("discard");
    assert!(version.section("s3.1.x1").is_none());
    assert!(version.retired_sections.contains(&"s3.1.x1".to_owned()));
}

#[test]
fn an_ask_grant_may_promote_its_answers() {
    let mut version = written();
    let mode = GrantMode::Ask {
        section_id: "s3.1".to_owned(),
    };
    run(
        &mut version,
        &mode,
        json!([
            {"op": "add_expansion", "under": "s3.1", "id": "s3.1.a", "title": "Answer"},
            {"op": "promote", "section": "s3.1.a"}
        ]),
    )
    .expect("ask");
    assert_eq!(
        version.section("s3.1.a").expect("answer").state,
        SectionState::Written
    );
}

#[test]
fn refresh_verify_and_area_grants_allow_only_their_operations() {
    let mut version = written();
    let refresh = GrantMode::Refresh {
        section_ids: vec!["s1".to_owned()],
    };
    run(
        &mut version,
        &refresh,
        json!([{"op": "put_section", "id": "s1", "body": "Fresh."}]),
    )
    .expect("refresh");
    assert!(
        run(
            &mut version,
            &refresh,
            json!([{"op": "put_section", "id": "s3.1", "body": "No."}])
        )
        .is_err()
    );

    run(
        &mut version,
        &GrantMode::Verify,
        json!([{"op": "record_checks", "section": "s3.1", "checks": [
            {"claim": "refilled on success", "status": "unsupported", "note": "refill is timed", "citations": ["c2"]}
        ]}]),
    )
    .expect("verify");
    let check = &version.section("s3.1").expect("step").checks[0];
    assert_eq!(check.status, ClaimStatus::Unsupported);
    assert_eq!(check.by_session.as_deref(), Some("session"));
    assert!(
        run(
            &mut version,
            &GrantMode::Verify,
            json!([{"op": "remove_section", "id": "s4"}])
        )
        .is_err()
    );

    let area = GrantMode::Area {
        area_id: "net".to_owned(),
    };
    let errors = run(
        &mut version,
        &area,
        json!([{"op": "put_section", "id": "s1", "body": "x"}]),
    )
    .expect_err("outside");
    assert_eq!(errors[0].code, OUTSIDE_GRANT);
}

#[test]
fn generate_moves_planned_sections_but_restructure_is_needed_for_written_ones() {
    let mut version = written();
    run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "move_section", "id": "s4", "parent": null, "after": null}]),
    )
    .expect("planned sections move");
    assert_eq!(version.outline[0].id, "s4");
    let errors = run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "set_outline", "sections": [{"id": "s3", "children": [{"id": "s3.1"}]}]}]),
    )
    .expect_err("dropping written sections");
    assert_eq!(errors[0].code, OUTSIDE_GRANT);
    run(
        &mut version,
        &GrantMode::Restructure,
        json!([
            {"op": "move_section", "id": "s3.1", "parent": null, "after": "s1"},
            {"op": "remove_section", "id": "s3"}
        ]),
    )
    .expect("restructure");
    let ids: Vec<&str> = version
        .outline
        .iter()
        .map(|section| section.id.as_str())
        .collect();
    assert_eq!(ids, ["s4", "s1", "s3.1"]);
    let errors = run(
        &mut version,
        &GrantMode::Restructure,
        json!([{"op": "put_section", "id": "s3", "kind": "note", "title": "Reused"}]),
    )
    .expect_err("a removed id is retired");
    assert!(
        errors[0].message.contains("not used again"),
        "{}",
        errors[0].message
    );
}

#[test]
fn citations_by_id_are_reused_and_unused_ones_dropped() {
    let mut version = written();
    run(
        &mut version,
        &GrantMode::Generate,
        json!([
            {"op": "put_section", "id": "s4", "body": "Races [[c2]].", "citations": ["c2"],
             "findings": [{"kind": "risk", "text": "Refill races.", "citations": ["c2"]}]},
            {"op": "put_section", "id": "s3.1", "body": "A bucket [[c1]].", "citations": ["c1"], "findings": []}
        ]),
    )
    .expect("reuse");
    assert!(version.citations.contains_key("c2"), "still used by s4");
    let errors = run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "put_section", "id": "s1", "citations": ["c99"]}]),
    )
    .expect_err("unknown id");
    assert!(errors[0].message.contains("c99"));
    run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "put_section", "id": "s4", "body": "", "citations": [], "findings": []}]),
    )
    .expect("uncite");
    assert!(!version.citations.contains_key("c2"), "nothing uses it");
}

#[test]
fn a_wanted_citation_id_must_be_free_or_name_the_same_lines() {
    let mut version = written();
    let errors = run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "put_section", "id": "s1", "citations": [
            {"id": "c1", "path": "other.rs", "start_line": 1, "end_line": 1}
        ]}]),
    )
    .expect_err("taken");
    assert!(errors[0].message.contains("c1"));
    run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "put_section", "id": "s1", "citations": [
            {"id": "budget", "path": "net/budget.rs", "start_line": 1, "end_line": 3, "side": "old"}
        ]}]),
    )
    .expect("free id");
    assert_eq!(version.citations["budget"].side, ReviewLineSide::Old);
}

#[test]
fn status_is_set_and_area_results_go_to_their_own_write() {
    let mut version = written();
    run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "set_status", "status": "failed"}]),
    )
    .expect("status");
    assert!(matches!(version.status, GuideStatus::Failed { .. }));
    let errors = run(
        &mut version,
        &GrantMode::Generate,
        json!([{"op": "area_result", "area_id": "net", "sections": []}]),
    )
    .expect_err("area results are not version writes");
    assert_eq!(errors[0].code, INVALID_OP);
}
