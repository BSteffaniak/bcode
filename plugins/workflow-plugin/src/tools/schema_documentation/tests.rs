use super::*;
use crate::tools::{delegation, portable_task_definition, task_group};

fn embedded(tool: &bcode_tool::ToolDefinition) -> Value {
    let description = tool.input_schema["properties"]["request_json"]["description"]
        .as_str()
        .unwrap();
    serde_json::from_str(
        description
            .strip_prefix("JSON-encoded task request. Payload contract: ")
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn product_documentation_expands_exactly_and_reduces_serialized_tools() {
    for original in [task_group::definition(), delegation::definition()] {
        let compacted = compact(&original.input_schema);
        assert_eq!(
            bcode_model_schema::inline_local_references(&compacted, EXPANSION_BUDGET).unwrap(),
            original.input_schema
        );
        assert!(compacted.to_string().len() < original.input_schema.to_string().len());
        assert_eq!(compact(&original.input_schema), compacted);
        let wrapped = portable_task_definition(original.clone());
        assert_eq!(wrapped.name, original.name);
        assert_eq!(wrapped.description, original.description);
        assert_eq!(embedded(&wrapped), compacted);
        let mut expanded = wrapped.clone();
        expanded.input_schema["properties"]["request_json"]["description"] = json!(format!(
            "JSON-encoded task request. Payload contract: {}",
            original.input_schema
        ));
        let factored_bytes = serde_json::to_vec(&wrapped).unwrap().len();
        let expanded_bytes = serde_json::to_vec(&expanded).unwrap().len();
        assert!(factored_bytes < expanded_bytes);
        eprintln!(
            "{} documentation bytes: expanded={expanded_bytes}, factored={factored_bytes}, saved={}",
            wrapped.name,
            expanded_bytes - factored_bytes,
        );
    }
}

#[test]
fn literal_data_and_unequal_constraints_are_not_shared() {
    let large = "documentation ".repeat(40);
    let schema = json!({"type":"object","properties":{
        "left":{"type":"string","description":large,"minLength":1},
        "right":{"type":"string","description":large,"minLength":2}
    },"examples":[{"properties":{"left":{"description":large}}}]});
    assert_eq!(compact(&schema), schema);
}

#[test]
fn unsafe_or_unprofitable_factoring_retains_expanded_fallback() {
    for schema in [
        json!({"$defs":{"existing":{"type":"string"}},"type":"object"}),
        json!({"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}}}),
        json!({"type":"object","properties":{
            "a":{"$ref":"https://invalid.example/schema","description":"x".repeat(400)},
            "b":{"$ref":"https://invalid.example/schema","description":"x".repeat(400)}
        }}),
    ] {
        assert_eq!(compact(&schema), schema);
    }
}

#[test]
fn contract_validation_preserves_positive_and_negative_requests() {
    for schema in [
        task_group::definition().input_schema,
        delegation::definition().input_schema,
    ] {
        let factored = compact(&schema);
        let original = jsonschema::validator_for(&schema).unwrap();
        let compacted = jsonschema::validator_for(&factored).unwrap();
        let request = if schema["properties"].get("version").is_some() {
            json!({
                "run_id":"run","expected_revision":1,"mutation_id":"documentation-test",
                "version":2,"generated_ids":true,"first_edge_id":1,
                "input":{"type_name":"input","schema":{"type":"string"}},
                "tasks":[{"task_id":"worker","objective":"inspect","agent_profile":"plan",
                    "output":{"type_name":"result","schema":{"type":"string"}}}],
                "continuation":{"objective":"verify","agent_profile":"plan",
                    "output":{"type_name":"result","schema":{"type":"string"}}},
                "reconciliation":[]
            })
        } else {
            json!({
                "run_id":"run","expected_revision":1,"mutation_id":"documentation-test",
                "bind_source_activation":"activation",
                "tasks":[{"task_id":"worker","objective":"inspect","agent_profile":"plan",
                    "output":{"type_name":"result","schema":{"type":"string"}}}],
                "continuation":{"objective":"verify","agent_profile":"plan"},
                "reconciliation":[]
            })
        };
        let positive = request;
        let errors: Vec<_> = original
            .iter_errors(&positive)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "contract errors: {errors:?}");
        assert!(compacted.is_valid(&positive));
        if schema["properties"].get("version").is_some() {
            let direct = crate::tools::parse_tool_edit(crate::tools::GROUP_NAME, &positive);
            let encoded = crate::tools::parse_tool_edit(
                crate::tools::GROUP_NAME,
                &json!({"request_json":positive.to_string()}),
            );
            assert_eq!(direct.unwrap(), encoded.unwrap());
            let mut parser_invalid = positive.clone();
            parser_invalid["bind_source_activation"] = json!("activation");
            assert!(original.is_valid(&parser_invalid));
            assert!(compacted.is_valid(&parser_invalid));
            let direct = crate::tools::parse_tool_edit(crate::tools::GROUP_NAME, &parser_invalid);
            let encoded = crate::tools::parse_tool_edit(
                crate::tools::GROUP_NAME,
                &json!({"request_json":parser_invalid.to_string()}),
            );
            assert_eq!(direct.unwrap_err(), encoded.unwrap_err());
        }
        let mut missing = positive.clone();
        missing.as_object_mut().unwrap().remove("run_id");
        let mut wrong = positive.clone();
        wrong["expected_revision"] = json!(0);
        for negative in [missing, wrong, json!([])] {
            assert!(!original.is_valid(&negative));
            assert!(!compacted.is_valid(&negative));
        }
    }
}

#[test]
fn strict_provider_schema_pipeline_preserves_compact_description_as_literal() {
    for tool in [task_group::definition(), delegation::definition()] {
        let wrapped = portable_task_definition(tool);
        let normalized = bcode_model_schema::normalize(
            &wrapped.input_schema,
            &bcode_model_schema::SchemaDialect {
                unsupported_keywords: BTreeMap::from([(
                    "default".into(),
                    bcode_model_schema::UnsupportedKeywordPolicy::Remove,
                )]),
                object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
                one_of: bcode_model_schema::OneOfPolicy::CollapseAnnotatedConstants,
                reference_siblings:
                    bcode_model_schema::ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
                ..bcode_model_schema::SchemaDialect::default()
            },
        )
        .unwrap();
        let wire =
            bcode_model_schema::inline_local_references(&normalized, EXPANSION_BUDGET).unwrap();
        assert_eq!(wire, wrapped.input_schema);
        let validator = jsonschema::validator_for(&wire).unwrap();
        assert!(validator.is_valid(&json!({"request_json":"{}"})));
        assert!(!validator.is_valid(&json!({"request_json":{}})));
        assert!(!validator.is_valid(&json!({"request_json":"{}","extra":true})));
    }
}
