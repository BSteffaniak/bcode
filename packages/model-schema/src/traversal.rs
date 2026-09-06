//! JSON Schema applicator traversal, distinct from provider portability policy.
use crate::{SchemaPortabilityError, join_pointer};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Draft {
    Seven,
    Nineteen,
    Twenty,
}

impl Draft {
    pub(super) fn from_schema(schema: &Value) -> Result<Self, SchemaPortabilityError> {
        if schema
            .get("$schema")
            .is_some_and(|value| !value.is_string())
        {
            return Err(SchemaPortabilityError::new(
                "/$schema",
                "dialect declaration must be a string",
            ));
        }
        let declaration = schema
            .get("$schema")
            .and_then(Value::as_str)
            .map(|uri| uri.trim_end_matches('#'));
        match declaration {
            None | Some("https://json-schema.org/draft/2020-12/schema") => Ok(Self::Twenty),
            Some("https://json-schema.org/draft/2019-09/schema") => Ok(Self::Nineteen),
            Some(
                "http://json-schema.org/draft-07/schema"
                | "https://json-schema.org/draft-07/schema",
            ) => Ok(Self::Seven),
            Some(uri) => Err(SchemaPortabilityError::new(
                "/$schema",
                format!("unsupported JSON Schema dialect `{uri}`"),
            )),
        }
    }
}

/// Return relative JSON pointers to immediate subschemas. Literal annotations,
/// property dependency arrays and unknown extension keywords are never traversed.
/// Draft-less inputs follow 2020-12 (the schema generator's default).
pub fn children(schema: &Value, draft: Draft) -> Vec<String> {
    let Some(object) = schema.as_object() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (key, value) in object {
        let path = join_pointer("", key);
        match key.as_str() {
            "properties" | "patternProperties" => map_children(value, &path, false, &mut paths),
            "definitions" | "dependencies" if draft != Draft::Twenty => {
                map_children(value, &path, key == "dependencies", &mut paths);
            }
            "$defs" | "dependentSchemas" if draft != Draft::Seven => {
                map_children(value, &path, false, &mut paths);
            }
            "allOf" | "anyOf" | "oneOf" => array_children(value, &path, &mut paths),
            "prefixItems" if draft == Draft::Twenty => array_children(value, &path, &mut paths),
            "items" if value.is_array() && draft != Draft::Twenty => {
                array_children(value, &path, &mut paths);
            }
            "items"
            | "additionalProperties"
            | "contains"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else" => paths.push(path),
            "additionalItems" if draft != Draft::Twenty => paths.push(path),
            "unevaluatedItems" | "unevaluatedProperties" | "contentSchema"
                if draft != Draft::Seven =>
            {
                paths.push(path);
            }
            _ => {}
        }
    }
    paths
}
fn map_children(value: &Value, path: &str, schemas_only: bool, paths: &mut Vec<String>) {
    if let Some(map) = value.as_object() {
        paths.extend(
            map.iter()
                .filter(|(_, value)| !schemas_only || value.is_object() || value.is_boolean())
                .map(|(key, _)| join_pointer(path, key)),
        );
    }
}
fn array_children(value: &Value, path: &str, paths: &mut Vec<String>) {
    if let Some(array) = value.as_array() {
        paths.extend((0..array.len()).map(|index| join_pointer(path, &index.to_string())));
    }
}
