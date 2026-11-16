//! Deterministic tool arguments derived from a tool's input schema.
//!
//! Generated calls satisfy the tool's canonical schema the way a real model's would: every
//! required property gets a placeholder of its type, and open objects also carry the fake
//! provider's correlation fields. Under strict tool schemas every optional property is sent as
//! `null`, as strict provider dialects require for an omitted argument, so hosts exercise the
//! same restoration they need for real strict providers.
use serde_json::{Map, Value, json};

/// Nesting bound for generated values and followed local references.
const MAX_DEPTH: usize = 32;

/// Arguments for `tool` that satisfy its input schema.
///
/// `correlation` fields (for example the fake call index) are added only where the root object
/// accepts undeclared properties.
pub fn fake_tool_arguments(
    tool: &bcode_model::ToolDefinition,
    strict: bool,
    correlation: &Map<String, Value>,
) -> Value {
    let schema = &tool.input_schema;
    let mut arguments = match instance(schema, schema, strict, 0) {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
        for (key, value) in correlation {
            arguments
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
    }
    Value::Object(arguments)
}

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(schema)
}

fn non_null_branch(schema: &Value) -> Option<&Value> {
    ["anyOf", "oneOf"].into_iter().find_map(|keyword| {
        schema
            .get(keyword)?
            .as_array()?
            .iter()
            .find(|branch| branch.get("type").and_then(Value::as_str) != Some("null"))
    })
}

fn schema_type(schema: &Value) -> Option<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => Some(kind),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ if schema.get("properties").is_some() => Some("object"),
        _ => None,
    }
}

fn instance(root: &Value, schema: &Value, strict: bool, depth: usize) -> Value {
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    let schema = resolve(root, schema);
    if let Some(value) = schema.get("const") {
        return value.clone();
    }
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return value.clone();
    }
    if let Some(branch) = non_null_branch(schema) {
        return instance(root, branch, strict, depth + 1);
    }
    match schema_type(schema) {
        Some("string") => json!("fake"),
        Some("integer" | "number") => json!(0),
        Some("boolean") => json!(false),
        Some("array") => {
            let minimum = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            let item = schema.get("items").map_or(Value::Null, |items| {
                instance(root, items, strict, depth + 1)
            });
            Value::Array(std::iter::repeat_n(item, usize::try_from(minimum).unwrap_or(0)).collect())
        }
        Some("object") => object_instance(root, schema, strict, depth),
        _ => Value::Null,
    }
}

fn object_instance(root: &Value, schema: &Value, strict: bool, depth: usize) -> Value {
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut object = Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, property) in properties {
            if required.contains(&key.as_str()) {
                object.insert(key.clone(), instance(root, property, strict, depth + 1));
            } else if strict {
                object.insert(key.clone(), Value::Null);
            }
        }
    }
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(input_schema: Value) -> bcode_model::ToolDefinition {
        bcode_model::ToolDefinition {
            name: "tool".to_owned(),
            description: "test".to_owned(),
            input_schema,
        }
    }

    fn index() -> Map<String, Value> {
        Map::from_iter([("index".to_owned(), json!(3))])
    }

    #[test]
    fn required_properties_get_typed_placeholders_and_closed_objects_stay_closed() {
        let closed = tool(json!({
            "type": "object",
            "required": ["path", "mode", "lines", "nested"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "mode": {"enum": ["read", "write"]},
                "lines": {"type": "array", "minItems": 1, "items": {"type": "integer"}},
                "nested": {"$ref": "#/$defs/Nested"},
                "note": {"type": "string"}
            },
            "$defs": {"Nested": {"type": "object", "required": ["flag"],
                "properties": {"flag": {"type": "boolean"}}}}
        }));

        assert_eq!(
            fake_tool_arguments(&closed, false, &index()),
            json!({"path": "fake", "mode": "read", "lines": [0], "nested": {"flag": false}})
        );
        assert_eq!(
            fake_tool_arguments(&closed, true, &index())["note"],
            Value::Null
        );
    }

    #[test]
    fn open_schemas_keep_the_correlation_fields() {
        assert_eq!(
            fake_tool_arguments(&tool(json!({"type": "object"})), true, &index()),
            json!({"index": 3})
        );
    }
}
