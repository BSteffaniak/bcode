//! Reversible map adaptation for closed-object structured-output dialects.
//!
//! Canonical schemas and values remain unchanged. Dynamic string-keyed maps use
//! arrays of closed key/value records only on the provider wire.
use crate::{SchemaPortabilityError, traversal};
use serde_json::{Value, json};

/// A compiled, reversible wire representation of a canonical output schema.
#[derive(Debug, Clone)]
pub struct ObjectMapEncoding {
    schema: Value,
    canonical: Value,
    adapted: bool,
}

impl ObjectMapEncoding {
    /// Compile dynamic maps to key/value arrays without changing the canonical schema.
    ///
    /// # Errors
    /// Rejects map shapes whose semantics cannot be represented losslessly, including
    /// mixed fixed/dynamic properties and maps underneath non-nullable composition.
    pub fn compile(schema: &Value) -> Result<Self, SchemaPortabilityError> {
        let draft = traversal::Draft::from_schema(schema)?;
        let mut wire = schema.clone();
        let adapted = encode_schema(&mut wire, draft, "")?;
        if adapted {
            crate::normalize(schema, &crate::SchemaDialect::default())?;
            if wire.get("type").and_then(Value::as_str) != Some("object") {
                return Err(SchemaPortabilityError::new(
                    "",
                    "encoded structured output requires an object envelope",
                ));
            }
            validate_composition(schema, draft, "")?;
        }
        Ok(Self {
            schema: wire,
            canonical: schema.clone(),
            adapted,
        })
    }

    /// The provider-wire schema, before provider-specific dialect normalization.
    #[must_use]
    pub const fn schema(&self) -> &Value {
        &self.schema
    }

    /// Whether output values need decoding.
    #[must_use]
    pub const fn is_adapted(&self) -> bool {
        self.adapted
    }

    /// Decode a provider value into the canonical representation.
    ///
    /// # Errors
    /// Rejects malformed records, duplicate keys, unsupported references and excessive
    /// nesting. The caller must still validate the result against the canonical schema.
    pub fn decode(&self, value: Value) -> Result<Value, SchemaPortabilityError> {
        decode_value(&self.canonical, &self.canonical, value, "", 0)
    }
}

fn validate_composition(
    schema: &Value,
    draft: traversal::Draft,
    path: &str,
) -> Result<(), SchemaPortabilityError> {
    if [
        "allOf",
        "oneOf",
        "if",
        "then",
        "else",
        "not",
        "patternProperties",
        "dependentSchemas",
        "dependencies",
        "prefixItems",
        "contains",
        "unevaluatedProperties",
        "unevaluatedItems",
        "additionalItems",
    ]
    .iter()
    .any(|key| schema.get(key).is_some())
        || schema.get("items").is_some_and(Value::is_array)
    {
        return Err(SchemaPortabilityError::new(
            path,
            "unsupported composition in map encoding",
        ));
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array)
        && !(branches.len() == 2
            && branches
                .iter()
                .filter(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
                .count()
                == 1)
    {
        return Err(SchemaPortabilityError::new(
            path,
            "map encoding supports only nullable anyOf unions",
        ));
    }
    for child in traversal::children(schema, draft) {
        validate_composition(
            schema.pointer(&child).expect("existing child"),
            draft,
            &format!("{path}{child}"),
        )?;
    }
    Ok(())
}

fn map_value(schema: &Value) -> Option<&Value> {
    schema
        .get("additionalProperties")
        .filter(|value| value.is_object())
}

fn encode_schema(
    schema: &mut Value,
    draft: traversal::Draft,
    path: &str,
) -> Result<bool, SchemaPortabilityError> {
    let mut adapted = false;
    for child in traversal::children(schema, draft) {
        adapted |= encode_schema(
            schema.pointer_mut(&child).expect("existing child"),
            draft,
            &format!("{path}{child}"),
        )?;
    }
    if map_value(schema).is_some() {
        let object = schema.as_object().expect("map schema");
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "additionalProperties" | "description" | "title" | "$comment" | "default"
            )
        }) {
            return Err(SchemaPortabilityError::new(
                path,
                "dynamic maps with additional constraints or fixed properties cannot be encoded losslessly",
            ));
        }
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(SchemaPortabilityError::new(
                path,
                "dynamic map requires an explicit object type",
            ));
        }
        let value = schema["additionalProperties"].clone();
        let description = schema.get("description").cloned();
        *schema = json!({"type":"array", "items":{"type":"object", "properties":{"key":{"type":"string"},"value":value},"required":["key","value"],"additionalProperties":false}});
        if let Some(description) = description {
            schema["description"] = description;
        }
        adapted = true;
    }
    Ok(adapted)
}

fn decode_map(
    root: &Value,
    item_schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
) -> Result<Value, SchemaPortabilityError> {
    let entries = value
        .as_array()
        .ok_or_else(|| SchemaPortabilityError::new(path, "encoded map must be an array"))?;
    let mut map = serde_json::Map::new();
    for entry in entries {
        let record = entry
            .as_object()
            .filter(|record| record.len() == 2 && record.contains_key("value"))
            .ok_or_else(|| {
                SchemaPortabilityError::new(path, "encoded map requires exact key/value records")
            })?;
        let key = record
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| SchemaPortabilityError::new(path, "encoded map key must be a string"))?;
        let decoded = decode_value(root, item_schema, record["value"].clone(), path, depth + 1)?;
        if map.insert(key.to_owned(), decoded).is_some() {
            return Err(SchemaPortabilityError::new(
                path,
                "duplicate encoded map key",
            ));
        }
    }
    Ok(Value::Object(map))
}

fn decode_value(
    root: &Value,
    schema: &Value,
    value: Value,
    path: &str,
    depth: usize,
) -> Result<Value, SchemaPortabilityError> {
    if depth > 128 {
        return Err(SchemaPortabilityError::new(
            path,
            "structured output nesting exceeds decoding limit",
        ));
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
            .ok_or_else(|| {
                SchemaPortabilityError::new(path, "unresolved map encoding reference")
            })?;
        return decode_value(root, target, value, path, depth + 1);
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if branches.len() == 2
            && branches
                .iter()
                .any(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
        {
            if value.is_null() {
                return Ok(value);
            }
            let branch = branches
                .iter()
                .find(|branch| branch.get("type").and_then(Value::as_str) != Some("null"))
                .expect("non-null branch");
            return decode_value(root, branch, value, path, depth + 1);
        }
        return Err(SchemaPortabilityError::new(
            path,
            "map decoding supports only nullable anyOf unions",
        ));
    }
    if ["allOf", "oneOf", "if", "patternProperties"]
        .iter()
        .any(|key| schema.get(key).is_some())
    {
        return Err(SchemaPortabilityError::new(
            path,
            "unsupported composition in map decoding",
        ));
    }
    if let Some(item_schema) = map_value(schema) {
        return decode_map(root, item_schema, &value, path, depth);
    }
    match value {
        Value::Object(mut object) => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, child_schema) in properties {
                    if let Some(child) = object.remove(key) {
                        object.insert(
                            key.clone(),
                            decode_value(
                                root,
                                child_schema,
                                child,
                                &crate::join_pointer(path, key),
                                depth + 1,
                            )?,
                        );
                    }
                }
            }
            Ok(Value::Object(object))
        }
        Value::Array(values) if schema.get("items").is_some() => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                decode_value(
                    root,
                    &schema["items"],
                    value,
                    &crate::join_pointer(path, &index.to_string()),
                    depth + 1,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_round_trip_and_duplicates_fail_without_changing_source() {
        let schema = json!({"type":"object","properties":{"files":{"type":"object","additionalProperties":{"type":"string"}}}});
        let encoding = ObjectMapEncoding::compile(&schema).unwrap();
        assert_eq!(schema["properties"]["files"]["type"], "object");
        assert_eq!(encoding.schema()["properties"]["files"]["type"], "array");
        assert_eq!(
            encoding
                .decode(json!({"files":[{"key":"a/b~c","value":"bytes"}]}))
                .unwrap(),
            json!({"files":{"a/b~c":"bytes"}})
        );
        for value in [
            json!({"files":[{"key":"a","value":"one"},{"key":"a","value":"two"}]}),
            json!({"files":[{"key":"a"}]}),
            json!({"files":{}}),
        ] {
            assert!(encoding.decode(value).is_err());
        }
        assert!(
            crate::normalize(
                &schema,
                &crate::SchemaDialect {
                    object_properties: crate::ObjectPropertyPolicy::RequireAllAndClose,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn nested_nullable_referenced_maps_decode() {
        let schema = json!({"$defs":{"Map":{"type":"object","additionalProperties":{"type":"object","additionalProperties":{"type":"string"}}}},"type":"object","properties":{"data":{"anyOf":[{"$ref":"#/$defs/Map"},{"type":"null"}]}}});
        let encoding = ObjectMapEncoding::compile(&schema).unwrap();
        assert_eq!(
            encoding.decode(json!({"data":null})).unwrap(),
            json!({"data":null})
        );
        assert_eq!(
            encoding
                .decode(json!({"data":[{"key":"outer","value":[{"key":"inner","value":"text"}]}]}))
                .unwrap(),
            json!({"data":{"outer":{"inner":"text"}}})
        );
    }

    #[test]
    fn unsupported_map_constraints_fail_before_dispatch() {
        for schema in [
            json!({"type":"object","properties":{"fixed":{"type":"string"}},"additionalProperties":{"type":"string"}}),
            json!({"type":"object","minProperties":1,"additionalProperties":{"type":"string"}}),
            json!({"anyOf":[{"type":"object","additionalProperties":{"type":"string"}},{"type":"string"}]}),
        ] {
            assert!(ObjectMapEncoding::compile(&schema).is_err());
        }
    }
}
