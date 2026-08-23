#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable JSON Schema normalization for model-provider output contracts.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

/// Policy for object properties in a provider JSON Schema dialect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectPropertyPolicy {
    /// Preserve the source schema's object-property declarations.
    #[default]
    Preserve,
    /// Require every declared property and reject undeclared properties.
    RequireAllAndClose,
}

/// Policy for a JSON Schema keyword unsupported by a provider dialect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedKeywordPolicy {
    /// Reject schemas containing the keyword.
    #[default]
    Reject,
    /// Remove the keyword while preserving the remaining structural schema.
    Remove,
}

/// Policy for siblings of a JSON Schema `$ref`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceSiblingPolicy {
    /// Preserve all siblings allowed by general JSON Schema.
    #[default]
    Preserve,
    /// Remove annotation-only siblings and reject semantic siblings whose behavior cannot be kept.
    RemoveAnnotationsRejectSemantic,
}

/// Policy for `oneOf` unions in a provider dialect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OneOfPolicy {
    /// Preserve general JSON Schema `oneOf` unions.
    #[default]
    Preserve,
    /// Collapse branches made only of `const` plus annotations into an equivalent `enum`.
    CollapseAnnotatedConstants,
}

/// Declarative provider JSON Schema dialect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDialect {
    /// Object-property normalization policy.
    pub object_properties: ObjectPropertyPolicy,
    /// Unsupported keywords and how each should be handled.
    #[serde(default)]
    pub unsupported_keywords: std::collections::BTreeMap<String, UnsupportedKeywordPolicy>,
    /// Accepted `minItems` values. Empty means every value is accepted.
    #[serde(default)]
    pub accepted_min_items: BTreeSet<u64>,
    /// Policy for `oneOf` unions.
    #[serde(default)]
    pub one_of: OneOfPolicy,
    /// Policy for keywords adjacent to a `$ref`.
    #[serde(default)]
    pub reference_siblings: ReferenceSiblingPolicy,
    /// Reject references outside the current schema document.
    #[serde(default = "default_true")]
    pub reject_external_references: bool,
    /// Reject reference cycles in the schema graph.
    #[serde(default = "default_true")]
    pub reject_recursive_references: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for SchemaDialect {
    fn default() -> Self {
        Self {
            object_properties: ObjectPropertyPolicy::Preserve,
            unsupported_keywords: std::collections::BTreeMap::new(),
            accepted_min_items: BTreeSet::new(),
            one_of: OneOfPolicy::Preserve,
            reference_siblings: ReferenceSiblingPolicy::Preserve,
            reject_external_references: true,
            reject_recursive_references: true,
        }
    }
}

/// Failure to represent a portable schema in a provider dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaPortabilityError {
    path: String,
    message: String,
}

impl SchemaPortabilityError {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            message: message.into(),
        }
    }

    /// JSON pointer identifying the rejected schema location.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Provider-portability failure description.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SchemaPortabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "schema at {}: {}", self.path, self.message)
    }
}

impl std::error::Error for SchemaPortabilityError {}

/// Normalize a JSON Schema into the declared provider dialect.
///
/// # Errors
///
/// Returns an error for rejected keywords, unsupported `minItems` values, external references,
/// missing local reference targets, or recursive local references.
pub fn normalize(schema: &Value, dialect: &SchemaDialect) -> Result<Value, SchemaPortabilityError> {
    let mut normalized = schema.clone();
    normalize_value(&mut normalized, dialect, "")?;
    validate_references(&normalized, dialect)?;
    Ok(normalized)
}

fn normalize_value(
    value: &mut Value,
    dialect: &SchemaDialect,
    path: &str,
) -> Result<(), SchemaPortabilityError> {
    match value {
        Value::Object(object) => normalize_object(object, dialect, path),
        Value::Array(values) => {
            for (index, child) in values.iter_mut().enumerate() {
                normalize_value(child, dialect, &join_pointer(path, &index.to_string()))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn normalize_object(
    object: &mut Map<String, Value>,
    dialect: &SchemaDialect,
    path: &str,
) -> Result<(), SchemaPortabilityError> {
    collapse_annotated_constant_one_of(object, dialect, path)?;
    normalize_reference_siblings(object, dialect, path)?;
    for (keyword, policy) in &dialect.unsupported_keywords {
        if object.contains_key(keyword) {
            match policy {
                UnsupportedKeywordPolicy::Reject => {
                    return Err(SchemaPortabilityError::new(
                        &join_pointer(path, keyword),
                        format!("keyword `{keyword}` is unsupported by this dialect"),
                    ));
                }
                UnsupportedKeywordPolicy::Remove => {
                    object.remove(keyword);
                }
            }
        }
    }
    if !dialect.accepted_min_items.is_empty()
        && let Some(min_items) = object.get("minItems")
    {
        let accepted = min_items
            .as_u64()
            .is_some_and(|value| dialect.accepted_min_items.contains(&value));
        if !accepted {
            return Err(SchemaPortabilityError::new(
                &join_pointer(path, "minItems"),
                "minItems value is unsupported by this dialect",
            ));
        }
    }
    if matches!(
        dialect.object_properties,
        ObjectPropertyPolicy::RequireAllAndClose
    ) && is_object_schema(object)
    {
        object.insert("additionalProperties".to_string(), Value::Bool(false));
        let properties = object
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let required = properties
            .as_object()
            .map(|properties| properties.keys().cloned().map(Value::String).collect())
            .unwrap_or_default();
        object.insert("required".to_string(), Value::Array(required));
    }
    let keys = object.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        if let Some(child) = object.get_mut(&key) {
            normalize_value(child, dialect, &join_pointer(path, &key))?;
        }
    }
    Ok(())
}

fn collapse_annotated_constant_one_of(
    object: &mut Map<String, Value>,
    dialect: &SchemaDialect,
    path: &str,
) -> Result<(), SchemaPortabilityError> {
    if dialect.one_of == OneOfPolicy::Preserve {
        return Ok(());
    }
    let Some(branches) = object.get("oneOf").and_then(Value::as_array) else {
        return Ok(());
    };
    let mut values = Vec::with_capacity(branches.len());
    for (index, branch) in branches.iter().enumerate() {
        let Some(branch) = branch.as_object() else {
            return Err(SchemaPortabilityError::new(
                &join_pointer(&join_pointer(path, "oneOf"), &index.to_string()),
                "oneOf branch is not an annotated constant",
            ));
        };
        if branch.keys().any(|key| {
            !matches!(
                key.as_str(),
                "const" | "type" | "description" | "title" | "$comment"
            )
        }) {
            return Err(SchemaPortabilityError::new(
                &join_pointer(&join_pointer(path, "oneOf"), &index.to_string()),
                "oneOf branch contains semantics that cannot be collapsed to enum",
            ));
        }
        let value = branch.get("const").ok_or_else(|| {
            SchemaPortabilityError::new(
                &join_pointer(&join_pointer(path, "oneOf"), &index.to_string()),
                "oneOf branch is missing const",
            )
        })?;
        values.push(value.clone());
    }
    object.remove("oneOf");
    object.insert("enum".to_owned(), Value::Array(values));
    Ok(())
}

fn normalize_reference_siblings(
    object: &mut Map<String, Value>,
    dialect: &SchemaDialect,
    path: &str,
) -> Result<(), SchemaPortabilityError> {
    if !object.contains_key("$ref")
        || dialect.reference_siblings == ReferenceSiblingPolicy::Preserve
    {
        return Ok(());
    }
    for annotation in ["description", "title", "$comment", "deprecated", "examples"] {
        object.remove(annotation);
    }
    if let Some(sibling) = object.keys().find(|key| key.as_str() != "$ref") {
        return Err(SchemaPortabilityError::new(
            &join_pointer(path, sibling),
            format!("semantic keyword `{sibling}` cannot accompany `$ref` in this dialect"),
        ));
    }
    Ok(())
}

fn is_object_schema(object: &Map<String, Value>) -> bool {
    object.get("type").and_then(Value::as_str) == Some("object")
        || object.contains_key("properties")
}

fn validate_references(
    schema: &Value,
    dialect: &SchemaDialect,
) -> Result<(), SchemaPortabilityError> {
    let mut stack = Vec::new();
    walk_references(schema, schema, dialect, "", &mut stack)
}

fn walk_references(
    root: &Value,
    value: &Value,
    dialect: &SchemaDialect,
    path: &str,
    stack: &mut Vec<String>,
) -> Result<(), SchemaPortabilityError> {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if reference.starts_with('#') {
                    let target_pointer = reference.strip_prefix('#').unwrap_or_default();
                    let target = root.pointer(target_pointer).ok_or_else(|| {
                        SchemaPortabilityError::new(
                            &join_pointer(path, "$ref"),
                            format!("local reference `{reference}` does not exist"),
                        )
                    })?;
                    if dialect.reject_recursive_references {
                        if stack.iter().any(|seen| seen == reference) {
                            return Err(SchemaPortabilityError::new(
                                &join_pointer(path, "$ref"),
                                format!("recursive reference `{reference}` is unsupported"),
                            ));
                        }
                        stack.push(reference.to_string());
                        walk_references(root, target, dialect, target_pointer, stack)?;
                        stack.pop();
                    }
                } else if dialect.reject_external_references {
                    return Err(SchemaPortabilityError::new(
                        &join_pointer(path, "$ref"),
                        "external references are unsupported by this dialect",
                    ));
                }
            }
            for (key, child) in object {
                if key != "$ref" {
                    walk_references(root, child, dialect, &join_pointer(path, key), stack)?;
                }
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                walk_references(
                    root,
                    child,
                    dialect,
                    &join_pointer(path, &index.to_string()),
                    stack,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn join_pointer(path: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{path}/{escaped}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn restrictive_dialect() -> SchemaDialect {
        SchemaDialect {
            object_properties: ObjectPropertyPolicy::RequireAllAndClose,
            unsupported_keywords: BTreeMap::from([
                ("minimum".to_string(), UnsupportedKeywordPolicy::Remove),
                ("maxLength".to_string(), UnsupportedKeywordPolicy::Reject),
            ]),
            accepted_min_items: BTreeSet::from([0, 1]),
            ..SchemaDialect::default()
        }
    }

    #[test]
    fn normalizes_nested_objects_and_removes_declared_keywords() {
        let normalized = normalize(
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "minimum": 0},
                    "child": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}}
                    }
                }
            }),
            &restrictive_dialect(),
        )
        .expect("schema should normalize");

        assert_eq!(normalized["additionalProperties"], false);
        assert_eq!(
            normalized["required"],
            serde_json::json!(["child", "count"])
        );
        assert!(normalized.pointer("/properties/count/minimum").is_none());
        assert_eq!(
            normalized.pointer("/properties/child/required"),
            Some(&serde_json::json!(["name"]))
        );
    }

    #[test]
    fn collapses_annotated_constant_one_of_and_rejects_semantic_branches() {
        let dialect = SchemaDialect {
            one_of: OneOfPolicy::CollapseAnnotatedConstants,
            ..SchemaDialect::default()
        };
        let normalized = normalize(
            &serde_json::json!({
                "oneOf": [
                    {"type": "string", "const": "high", "description": "High confidence"},
                    {"type": "string", "const": "low", "description": "Low confidence"}
                ]
            }),
            &dialect,
        )
        .expect("annotated constants should collapse");
        assert_eq!(normalized, serde_json::json!({"enum": ["high", "low"]}));

        let error = normalize(
            &serde_json::json!({
                "oneOf": [
                    {"type": "object", "properties": {"left": {"type": "string"}}},
                    {"type": "object", "properties": {"right": {"type": "string"}}}
                ]
            }),
            &dialect,
        )
        .expect_err("semantic unions must not be silently collapsed");
        assert_eq!(error.path(), "/oneOf/0");
    }

    #[test]
    fn removes_reference_annotations_and_rejects_semantic_siblings_when_required() {
        let dialect = SchemaDialect {
            reference_siblings: ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
            ..SchemaDialect::default()
        };
        let normalized = normalize(
            &serde_json::json!({
                "$defs": {"confidence": {"type": "string"}},
                "type": "object",
                "properties": {
                    "confidence": {
                        "$ref": "#/$defs/confidence",
                        "description": "Model-reported confidence"
                    }
                }
            }),
            &dialect,
        )
        .expect("annotation-only siblings should be removed");
        assert_eq!(
            normalized.pointer("/properties/confidence"),
            Some(&serde_json::json!({"$ref": "#/$defs/confidence"}))
        );

        let error = normalize(
            &serde_json::json!({
                "$defs": {"confidence": {"type": "string"}},
                "type": "object",
                "properties": {
                    "confidence": {
                        "$ref": "#/$defs/confidence",
                        "minLength": 1
                    }
                }
            }),
            &dialect,
        )
        .expect_err("semantic siblings must not be silently discarded");
        assert_eq!(error.path(), "/properties/confidence/minLength");
    }

    #[test]
    fn rejects_unsupported_keywords_and_min_items_values() {
        let error = normalize(
            &serde_json::json!({"type": "string", "maxLength": 8}),
            &restrictive_dialect(),
        )
        .expect_err("maxLength should be rejected");
        assert_eq!(error.path(), "/maxLength");

        let error = normalize(
            &serde_json::json!({"type": "array", "minItems": 2}),
            &restrictive_dialect(),
        )
        .expect_err("minItems=2 should be rejected");
        assert_eq!(error.path(), "/minItems");
    }

    #[test]
    fn accepts_internal_references_and_rejects_external_or_recursive_references() {
        normalize(
            &serde_json::json!({
                "$defs": {"item": {"type": "string"}},
                "type": "array",
                "items": {"$ref": "#/$defs/item"}
            }),
            &SchemaDialect::default(),
        )
        .expect("acyclic internal reference should pass");

        let external = normalize(
            &serde_json::json!({"$ref": "https://example.com/schema.json"}),
            &SchemaDialect::default(),
        )
        .expect_err("external reference should fail");
        assert_eq!(external.path(), "/$ref");

        let recursive = normalize(
            &serde_json::json!({
                "$defs": {"node": {"$ref": "#/$defs/node"}},
                "$ref": "#/$defs/node"
            }),
            &SchemaDialect::default(),
        )
        .expect_err("recursive reference should fail");
        assert!(recursive.message().contains("recursive"));
    }
}
