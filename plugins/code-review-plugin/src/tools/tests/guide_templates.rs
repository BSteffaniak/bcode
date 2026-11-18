//! Guide templates through the plugin entry point: configured required sections are enforced
//! by the write tool's lint, `freeform` accepts a purpose-only guide, and a file-set review
//! without a diff is explained with in-review and outside-review citations.

use super::{Harness, SESSION};
use bcode_code_review::ReviewData;
use bcode_code_review::testing::fixture::GitFixture;
use bcode_code_review_models::{
    BindReviewAssistantRequest, REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
    ResolveReviewSourcesRequest, ReviewAssistantBinding, ReviewSourceKind,
};
use serde_json::{Value, json};

/// Bind `session` to a transient review of `kinds`.
fn bind_kinds(harness: &Harness, kinds: Vec<ReviewSourceKind>) {
    let workspace = bcode_code_review::scope::transient_workspace(
        harness.fixture.root().to_path_buf(),
        None,
        kinds,
    );
    let resolved = ReviewData::default()
        .resolve_sources(&ResolveReviewSourcesRequest {
            browse: bcode_code_review_models::BrowseOptions::default(),
            repo_path: harness.fixture.root().to_path_buf(),
            sources: workspace.sources.clone(),
            provider_resolutions: Vec::new(),
        })
        .expect("resolve");
    harness
        .store()
        .bind_assistant(BindReviewAssistantRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            binding: ReviewAssistantBinding {
                schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                session_id: SESSION.to_owned(),
                workspace,
                resolved: resolved.sources,
                stacks: bcode_code_review_models::ReviewAssistantStacks::from_stacks(
                    &resolved.stacks,
                ),
                created_at_ms: 0,
                updated_at_ms: 0,
                started: None,
                detached_at_ms: None,
                guide_grant: None,
            },
        })
        .expect("bind");
}

fn lint_codes(answer: &Value) -> Vec<(String, Option<String>)> {
    answer["lint"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    (
                        item["code"].as_str().unwrap_or_default().to_owned(),
                        item["section"].as_str().map(str::to_owned),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn missing(answer: &Value) -> Vec<String> {
    answer["lint"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["code"] == "missing_required")
        .filter_map(|item| item["message"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn a_configured_templates_required_sections_are_enforced_by_lint() {
    let mut harness = Harness::new();
    harness.guide = Some(json!({
        "templates": {
            "security": {
                "extends": "default",
                "required_sections": ["purpose", "scope", "reading_order", "threat_model"],
                "sections": [{"key": "threat_model", "title": "Threat model",
                              "prompt": "Who can reach the changed code.", "when": ["auth"]}]
            }
        }
    }));
    harness.bind(SESSION);
    let started = harness.json(
        "code_review.guide.start",
        &json!({
            "template": "security",
            "outline": [
                {"id": "s1", "kind": "purpose", "title": "Purpose"},
                {"id": "s2", "kind": "scope", "title": "Scope"},
                {"id": "s3", "kind": "reading_order", "title": "Reading order"}
            ]
        }),
        SESSION,
    );
    assert_eq!(started["template"]["name"], "security", "{started}");
    assert_eq!(started["template"]["sections"][0]["key"], "threat_model");
    let guide = started["guide_id"].as_str().expect("guide").to_owned();

    let written = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [
            {"op": "put_section", "id": "s1", "body": "Hardens the login path [[c9]]."},
            {"op": "put_section", "id": "s4", "kind": "risks", "title": "Risks", "body": "No risks found."},
            {"op": "put_section", "id": "s5", "kind": "topic", "key": "auth", "title": "Auth", "body": "Tokens rotate."}
        ]}),
        SESSION,
    );
    assert_eq!(written["written"], true, "lint never refuses: {written}");
    let wanted = missing(&written);
    assert_eq!(wanted.len(), 1, "{written}");
    assert!(wanted[0].contains("threat_model"), "{wanted:?}");
    let codes = lint_codes(&written);
    for expected in [
        ("unknown_citation_token", "s1"),
        ("boilerplate", "s4"),
        ("uncited_section", "s5"),
    ] {
        assert!(
            codes.contains(&(expected.0.to_owned(), Some(expected.1.to_owned()))),
            "{expected:?} in {codes:?}"
        );
    }

    let fixed = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [
            {"op": "put_section", "id": "s6", "kind": "threat_model", "title": "Threat model",
             "body": "Only signed-in users reach it."}
        ]}),
        SESSION,
    );
    assert!(missing(&fixed).is_empty(), "{fixed}");
}

#[test]
fn the_freeform_template_accepts_a_purpose_only_guide() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let started = harness.json(
        "code_review.guide.start",
        &json!({"template": "freeform", "outline": [{"id": "s1", "kind": "purpose", "title": "Purpose"}]}),
        SESSION,
    );
    assert_eq!(started["template"]["required_sections"], json!(["purpose"]));
    let guide = started["guide_id"].as_str().expect("guide").to_owned();
    let written = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [
            {"op": "put_section", "id": "s1", "body": "Renames the fixture helpers."},
            {"op": "set_status", "status": "complete"}
        ]}),
        SESSION,
    );
    assert_eq!(written["written"], true, "{written}");
    assert_eq!(
        written["lint"],
        json!([]),
        "a purpose-only freeform guide is complete"
    );

    let mut harness = Harness::new();
    harness.bind(SESSION);
    let started = harness.json(
        "code_review.guide.start",
        &json!({"outline": [{"id": "s1", "kind": "purpose", "title": "Purpose"}]}),
        SESSION,
    );
    let written = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": started["guide_id"], "ops": [
            {"op": "put_section", "id": "s1", "body": "Renames the fixture helpers."}
        ]}),
        SESSION,
    );
    assert_eq!(
        missing(&written).len(),
        2,
        "the default template wants scope and reading order: {written}"
    );
}

#[test]
fn a_file_set_review_is_explained_with_in_review_and_outside_review_citations() {
    let mut harness = Harness::new();
    let shown = GitFixture::modified_path(0);
    let other = GitFixture::modified_path(1);
    bind_kinds(
        &harness,
        vec![ReviewSourceKind::File {
            path: shown.clone(),
        }],
    );
    let quote = |path: &str| {
        harness
            .fixture
            .git(&["show", &format!("HEAD:{path}")])
            .lines()
            .next()
            .expect("a first line")
            .to_owned()
    };
    let (shown_quote, other_quote) = (quote(&shown), quote(&other));
    let plan = harness.json("code_review.guide.plan", &json!({}), SESSION);
    assert_eq!(
        plan["template"]["name"], "explain",
        "a review without a diff is explained: {plan}"
    );
    let started = harness.json(
        "code_review.guide.start",
        &json!({"outline": [
            {"id": "s1", "kind": "purpose", "title": "Purpose"},
            {"id": "s2", "kind": "entry_points", "title": "Entry points"}
        ]}),
        SESSION,
    );
    assert_eq!(started["template"]["name"], "explain", "{started}");
    let written = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": started["guide_id"], "ops": [
            {"op": "put_section", "id": "s2", "body": "Starts here [[1]], used from [[2]].",
             "citations": [
                {"path": shown, "start_line": 1, "end_line": 1, "quote": shown_quote},
                {"path": other, "start_line": 1, "end_line": 1, "quote": other_quote}
             ]}
        ]}),
        SESSION,
    );
    assert_eq!(written["written"], true, "{written}");
    assert_eq!(
        written["citations"][0]["relation"], "in_review",
        "{written}"
    );
    assert_eq!(
        written["citations"][1]["relation"], "outside_review",
        "{written}"
    );
}
