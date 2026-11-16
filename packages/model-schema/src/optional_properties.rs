//! Reversible representation of optional object properties for closed-object dialects.
//!
//! Dialects that require every declared property cannot express "leave this property out".
//! [`crate::ObjectPropertyPolicy::RequireAllNullableOptionalAndClose`] lets such a property
//! accept `null` on the provider wire instead, and [`omit_null_optional_properties`] restores
//! the canonical value by removing those `null`s again.
use serde_json::{Map, Value};

/// Nesting bound for walking values and following local references.
const MAX_DEPTH: usize = 128;

/// Remove `null` values that stand for omitted optional properties.
///
/// A property value is removed when it is `null`, the canonical `schema` does not list the
/// property in `required`, and the property's schema does not itself accept `null`. Such a
/// value can only mean "not given", so removing it never changes the meaning of a value the
/// canonical schema accepts. Nested objects and array items are restored recursively, local
/// `$ref`s are followed, and a nullable `anyOf` is followed through its non-null branch.
///
/// Parts of the value the schema does not describe unambiguously (other unions, unresolvable
/// references, or nesting beyond the walk bound) are returned unchanged; the caller still
/// validates the result against the canonical schema.
#[must_use]
pub fn omit_null_optional_properties(schema: &Value, value: Value) -> Value {
    restore(schema, schema, value, 0)
}

/// Whether `schema` declares an object property that is optional and does not accept `null`.
///
/// Values produced against a [`crate::ObjectPropertyPolicy::RequireAllNullableOptionalAndClose`]
/// normalization of such a schema may contain `null`s that [`omit_null_optional_properties`]
/// must remove before they are valid canonical values.
#[must_use]
pub fn has_optional_properties(schema: &Value) -> bool {
    let mut stack = Vec::new();
    declares_optional(schema, schema, &mut stack, 0)
}

/// Whether a schema accepts `null` by itself (without following references).
pub fn schema_allows_null(schema: &Value) -> bool {
    let object = match schema {
        Value::Bool(accepts) => return *accepts,
        Value::Object(object) => object,
        _ => return false,
    };
    if object.is_empty() {
        return true;
    }
    match object.get("type") {
        Some(Value::String(kind)) if kind == "null" => return true,
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "null") => return true,
        _ => {}
    }
    if object.get("const").is_some_and(Value::is_null)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(Value::is_null))
    {
        return true;
    }
    ["anyOf", "oneOf"].into_iter().any(|keyword| {
        object
            .get(keyword)
            .and_then(Value::as_array)
            .is_some_and(|branches| branches.iter().any(schema_allows_null))
    })
}

/// The declared `required` property names of an object schema.
pub fn required_names(object: &Map<String, Value>) -> Vec<&str> {
    object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> Option<&'a Value> {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
}

/// The single non-null branch of a nullable `anyOf`/`oneOf`, if the schema is one.
fn nullable_branch(schema: &Value) -> Option<&Value> {
    ["anyOf", "oneOf"].into_iter().find_map(|keyword| {
        let branches = schema.get(keyword)?.as_array()?;
        let mut non_null = branches.iter().filter(|branch| !schema_allows_null(branch));
        let branch = non_null.next()?;
        (non_null.next().is_none() && branches.len() == 2).then_some(branch)
    })
}

fn restore(root: &Value, schema: &Value, value: Value, depth: usize) -> Value {
    if depth > MAX_DEPTH || value.is_null() {
        return value;
    }
    if let Some(target) = resolve(root, schema) {
        return restore(root, target, value, depth + 1);
    }
    if let Some(branch) = nullable_branch(schema) {
        return restore(root, branch, value, depth + 1);
    }
    match value {
        Value::Object(mut object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return Value::Object(object);
            };
            let required = schema.as_object().map(required_names).unwrap_or_default();
            for (key, property) in properties {
                let Some(child) = object.remove(key) else {
                    continue;
                };
                let property = resolve(root, property).unwrap_or(property);
                if child.is_null()
                    && !required.contains(&key.as_str())
                    && !schema_allows_null(property)
                {
                    continue;
                }
                object.insert(key.clone(), restore(root, property, child, depth + 1));
            }
            Value::Object(object)
        }
        Value::Array(items) => match schema.get("items") {
            Some(items_schema) if items_schema.is_object() => Value::Array(
                items
                    .into_iter()
                    .map(|item| restore(root, items_schema, item, depth + 1))
                    .collect(),
            ),
            _ => Value::Array(items),
        },
        other => other,
    }
}

fn declares_optional<'a>(
    root: &'a Value,
    schema: &'a Value,
    stack: &mut Vec<&'a str>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let Some(object) = schema.as_object() else {
        return false;
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if stack.contains(&reference) {
            return false;
        }
        if let Some(target) = resolve(root, schema) {
            stack.push(reference);
            let found = declares_optional(root, target, stack, depth + 1);
            stack.pop();
            if found {
                return true;
            }
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        let required = required_names(object);
        for (key, property) in properties {
            let resolved = resolve(root, property).unwrap_or(property);
            if !required.contains(&key.as_str()) && !schema_allows_null(resolved) {
                return true;
            }
            if declares_optional(root, property, stack, depth + 1) {
                return true;
            }
        }
    }
    ["items", "additionalProperties"]
        .into_iter()
        .filter_map(|keyword| object.get(keyword))
        .chain(
            ["anyOf", "oneOf", "allOf"]
                .into_iter()
                .filter_map(|keyword| object.get(keyword).and_then(Value::as_array))
                .flatten(),
        )
        .any(|child| declares_optional(root, child, stack, depth + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectPropertyPolicy, SchemaDialect, normalize};
    use serde_json::json;

    fn nullable_dialect() -> SchemaDialect {
        SchemaDialect {
            object_properties: ObjectPropertyPolicy::RequireAllNullableOptionalAndClose,
            ..SchemaDialect::default()
        }
    }

    fn worktree_like_schema() -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string"},
                "branch": {"type": "string", "description": "Existing branch"},
                "detach": {"type": "boolean"},
                "note": {"type": ["string", "null"]},
                "options": {
                    "type": "object",
                    "properties": {"depth": {"type": "integer"}}
                },
                "targets": {
                    "type": "array",
                    "items": {"$ref": "#/$defs/Target"}
                }
            },
            "$defs": {
                "Target": {
                    "type": "object",
                    "required": ["path"],
                    "properties": {"path": {"type": "string"}, "mode": {"type": "string"}}
                }
            }
        })
    }

    #[test]
    fn strict_wire_values_round_trip_to_canonical_values() {
        let schema = worktree_like_schema();
        let wire = normalize(&schema, &nullable_dialect()).expect("wire schema");
        let wire_validator = jsonschema::validator_for(&wire).expect("wire validator");
        let canonical_validator = jsonschema::validator_for(&schema).expect("canonical validator");
        let model_output = json!({
            "name": "feature",
            "branch": null,
            "detach": null,
            "note": null,
            "options": {"depth": null},
            "targets": [{"path": "a", "mode": null}]
        });
        assert!(wire_validator.is_valid(&model_output));
        assert!(!canonical_validator.is_valid(&model_output));

        let restored = omit_null_optional_properties(&schema, model_output);
        assert!(canonical_validator.is_valid(&restored));
        assert_eq!(
            restored,
            json!({
                "name": "feature",
                "note": null,
                "options": {},
                "targets": [{"path": "a"}]
            })
        );
    }

    #[test]
    fn required_and_nullable_nulls_and_unknown_shapes_are_kept() {
        let schema = json!({
            "type": "object",
            "required": ["must"],
            "properties": {
                "must": {"type": "string"},
                "maybe": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "either": {"anyOf": [{"type": "string"}, {"type": "integer"}]}
            }
        });
        let value = json!({"must": null, "maybe": null, "either": null, "extra": null});
        assert_eq!(
            omit_null_optional_properties(&schema, value.clone()),
            json!({"maybe": null, "must": null, "extra": null})
        );
        assert_eq!(
            omit_null_optional_properties(&json!(true), value.clone()),
            value
        );
    }

    #[test]
    fn nullable_wrapped_objects_restore_their_own_optionals() {
        let schema = json!({
            "type": "object",
            "properties": {
                "inner": {"anyOf": [
                    {"type": "object", "properties": {"flag": {"type": "boolean"}}},
                    {"type": "null"}
                ]}
            }
        });
        assert_eq!(
            omit_null_optional_properties(&schema, json!({"inner": {"flag": null}})),
            json!({"inner": {}})
        );
        assert_eq!(
            omit_null_optional_properties(&schema, json!({"inner": null})),
            json!({"inner": null})
        );
    }

    #[test]
    fn optional_property_detection_follows_nesting_and_references() {
        assert!(has_optional_properties(&worktree_like_schema()));
        assert!(!has_optional_properties(&json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {"a": {"type": "string"}, "b": {"type": ["integer", "null"]}}
        })));
        assert!(has_optional_properties(&json!({
            "type": "array",
            "items": {"$ref": "#/$defs/Item"},
            "$defs": {"Item": {"type": "object", "properties": {"x": {"type": "string"}}}}
        })));
        assert!(!has_optional_properties(&json!({
            "$defs": {"Node": {"type": "object", "required": ["next"],
                "properties": {"next": {"anyOf": [{"$ref": "#/$defs/Node"}, {"type": "null"}]}}}},
            "$ref": "#/$defs/Node"
        })));
    }
}
