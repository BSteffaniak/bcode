mod native;

use super::*;
use crate::tools::{context_definition, exact_output_view, parse_context};

#[test]
fn advertised_strict_schema_requires_nullable_projection_selector() {
    let schema = context_definition().input_schema;
    let properties = schema["properties"].as_object().unwrap();
    let required = schema["required"].as_array().unwrap();
    for property in properties.keys() {
        assert!(required.contains(&json!(property)));
    }
    assert_eq!(
        schema["properties"]["output_pointer"]["type"],
        json!(["string", "null"])
    );
}

fn context(value: Value) -> bcode_workflow::WorkflowExecutionContext {
    let mut fixture = json!({
        "run_id":"run", "node_id":"evaluator", "activation_id":"evaluation", "attempt":1,
        "graph":{"revision":7,"nodes":[],"edges":[],"nodes_complete":true,"edges_complete":true},
        "output":{
            "version":1,"output_id":"worker-result","run_id":"run","node_id":"worker",
            "activation_id":"worker-activation","schema_id":"contribution","schema_version":1,
            "checksum_sha256":"whole-value-checksum","created_at_ms":1,"value":null
        },"outputs":[]
    });
    fixture["output"]["value"] = value;
    serde_json::from_value(fixture).unwrap()
}

fn query(pointer: Value) -> Value {
    let mut arguments = json!({"output_only":true,"output_id":"worker-result"});
    arguments["output_pointer"] = pointer;
    arguments
}

fn projection(context: &bcode_workflow::WorkflowExecutionContext, pointer: &str) -> Value {
    serde_json::from_str(&render(context, pointer).unwrap()).unwrap()
}

#[test]
fn small_contribution_is_visible_without_inherited_state() {
    let context =
        context(json!({"summary":"validated current contribution", "history":"x".repeat(100_000)}));
    let output = projection(&context, "/summary");
    assert_eq!(
        output["projection"]["value"],
        "validated current contribution"
    );
    assert_eq!(output["projection"]["complete"], true);
    assert_eq!(output["output"]["checksum_sha256"], "whole-value-checksum");
    assert_eq!(
        output["output"]["checksum_scope"],
        "whole_canonical_value_not_projection"
    );
    assert_eq!(output["output"]["output_id"], "worker-result");
    assert_eq!(output["output"]["activation_id"], "worker-activation");
    assert!(output["output"].get("value").is_none());
    assert!(output.to_string().len() <= MAX_RESPONSE_BYTES);
}

#[test]
fn rfc6901_selection_preserves_null_and_missing_distinction() {
    let context = context(json!({"a/b":{"~key":[null,"結果"]},"":42}));
    for (pointer, expected) in [
        ("/a~1b/~0key/0", Value::Null),
        ("/a~1b/~0key/1", json!("結果")),
        ("/", json!(42)),
    ] {
        assert!(selector(&query(json!(pointer))).is_ok());
        let output = projection(&context, pointer);
        assert_eq!(output["projection"]["status"], "found");
        assert_eq!(output["projection"]["value"], expected);
        assert!(output["projection"].get("value").is_some());
    }
    let missing = projection(&context, "/absent");
    assert_eq!(missing["projection"]["status"], "missing");
    assert!(missing["projection"].get("value").is_none());
    assert_eq!(
        projection(&context, "")["projection"]["value"],
        context.output.unwrap().value
    );
}

#[test]
fn invalid_selectors_fail_closed_before_host_decoding() {
    for arguments in [
        json!({"output_pointer":"/summary"}),
        json!({"output_pointer":"/summary","output_id":"worker-result"}),
        json!({"output_pointer":"/summary","output_only":true,"output_id":""}),
        json!({"output_pointer":"/summary","output_only":true,"output_id":"worker-result","outputs_only":true}),
        json!({"output_pointer":"/summary","output_only":true,"output_id":"worker-result","delegation_only":true}),
        json!({"output_pointer":"/summary","output_only":true,"output_id":"worker-result","delegation_part":"input"}),
        query(json!(false)),
        query(json!("summary")),
        query(json!("/~")),
        query(json!("/~2")),
        query(json!(format!("/{}", "x".repeat(MAX_POINTER_BYTES)))),
    ] {
        assert!(parse_context(arguments).is_err());
    }
    assert!(parse_context(json!({"output_only":true,"output_id":"worker-result","output_pointer":"/summary","unknown":true})).is_err());
}

#[test]
fn selector_is_stripped_from_host_query_but_bound_to_preparation() {
    let arguments = query(json!("/summary"));
    let host = parse_context(arguments.clone()).unwrap();
    assert!(
        serde_json::to_value(host)
            .unwrap()
            .get("output_pointer")
            .is_none()
    );
    let pointer = selector(&arguments).unwrap();
    let descriptor = json!({"output_pointer":pointer});
    assert!(matches(&descriptor, Some("/summary")));
    assert!(!matches(&descriptor, Some("/history")));
    assert!(!matches(&descriptor, None));
    assert!(!matches(&json!({}), Some("/summary")));
    assert!(matches(&json!({}), None));
    assert!(matches(&json!({"output_pointer":null}), None));
    assert_eq!(
        selector(&query(Value::Null)).unwrap(),
        selector(&json!({"output_only":true,"output_id":"worker-result"})).unwrap()
    );
}

#[test]
fn nullable_selector_preserves_default_query_and_full_output() {
    let absent = json!({"output_only":true,"output_id":"worker-result"});
    assert_eq!(
        parse_context(absent.clone()).unwrap(),
        parse_context(query(Value::Null)).unwrap()
    );
    assert_eq!(selector(&absent).unwrap(), None);
    assert_eq!(selector(&query(Value::Null)).unwrap(), None);
    let context = context(json!({"summary":"current", "history":"retained"}));
    let output: Value = serde_json::from_str(&exact_output_view(&context)).unwrap();
    assert_eq!(
        output["output"],
        serde_json::to_value(context.output).unwrap()
    );
    assert!(output.get("projection").is_none());
}

#[test]
fn unavailable_output_and_oversized_metadata_fail_closed() {
    let mut context = context(json!({"summary":"current"}));
    context.output = None;
    let output = projection(&context, "/summary");
    assert_eq!(output["projection"]["status"], "output_unavailable");
    assert_eq!(output["projection"]["complete"], false);
    assert!(output["projection"].get("value").is_none());
    context.run_id = "x".repeat(MAX_RESPONSE_BYTES);
    assert!(render(&context, "/summary").is_err());
}

#[test]
fn oversized_utf8_selection_is_omitted_not_clipped_or_certified_complete() {
    let selected = json!("結果🦀".repeat(2_000));
    let expected_bytes = selected.to_string().len();
    let context = context(json!({"summary":selected}));
    let rendered = render(&context, "/summary").unwrap();
    assert!(rendered.len() <= MAX_RESPONSE_BYTES);
    let output: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(output["projection"]["selected_json_bytes"], expected_bytes);
    assert_eq!(output["projection"]["status"], "too_large");
    assert_eq!(output["projection"]["complete"], false);
    assert_eq!(output["projection"]["value_omitted"], true);
    assert!(output["projection"].get("value").is_none());
    assert_eq!(output["output"]["checksum_sha256"], "whole-value-checksum");
}
