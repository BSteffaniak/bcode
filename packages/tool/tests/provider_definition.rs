use bcode_tool::ToolDefinition;

#[test]
fn provider_definition_serialization_excludes_policy_and_presentation() {
    let definition = ToolDefinition {
        name: "example".to_owned(),
        description: "Example tool".to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
    };

    let value = serde_json::to_value(definition).expect("tool definition should serialize");
    let object = value
        .as_object()
        .expect("tool definition should be an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["description", "input_schema", "name"]
    );
    assert!(!object.contains_key("policy"));
    assert!(!object.contains_key("requires_permission"));
    assert!(!object.contains_key("ui"));
}
