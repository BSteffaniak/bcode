//! Conservative workflow schema policy for natural-language completion.

/// Only an explicitly annotated, unconstrained, non-secret prose string opts in.
/// Unknown strings and structured/path-like formats deliberately remain excluded.
pub fn is_prose_schema(schema: &serde_json::Value) -> bool {
    schema.get("type").and_then(serde_json::Value::as_str) == Some("string")
        && schema
            .get("contentMediaType")
            .and_then(serde_json::Value::as_str)
            == Some("text/plain")
        && schema.get("format").is_none()
        && !is_secret_schema(schema)
        && [
            "$ref",
            "allOf",
            "anyOf",
            "oneOf",
            "enum",
            "const",
            "contentEncoding",
        ]
        .iter()
        .all(|key| schema.get(key).is_none())
}

/// Reject secret or write-only schema fields at the terminal boundary.
pub fn is_secret_schema(schema: &serde_json::Value) -> bool {
    schema
        .get("writeOnly")
        .is_some_and(|value| value != &serde_json::Value::Bool(false))
        || schema.get("format").and_then(serde_json::Value::as_str) == Some("password")
}

/// Check an explicit nested prose annotation without resolving ambiguous schemas.
pub fn prose_field(schema: &serde_json::Value, path: &str) -> bool {
    let mut current = schema;
    for part in path.split('.') {
        if is_secret_schema(current)
            || ["$ref", "allOf", "anyOf", "oneOf"]
                .iter()
                .any(|key| current.get(key).is_some())
        {
            return false;
        }
        let Some(next) = current
            .get("properties")
            .and_then(|properties| properties.get(part))
        else {
            return false;
        };
        current = next;
    }
    is_prose_schema(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prose_requires_explicit_metadata_and_never_admits_secrets_or_other_controls() {
        assert!(is_prose_schema(
            &json!({"type":"string", "contentMediaType":"text/plain"})
        ));
        for schema in [
            json!({"type":"string"}),
            json!({"type":"string", "format":"path"}),
            json!({"type":"boolean", "contentMediaType":"text/plain"}),
            json!({"type":"integer", "contentMediaType":"text/plain"}),
            json!({"type":"string", "contentMediaType":"text/plain", "writeOnly":true}),
            json!({"type":"string", "contentMediaType":"text/plain", "format":"password"}),
            json!({"type":"string", "contentMediaType":"text/plain", "enum":["a"]}),
            json!({"type":"string", "contentMediaType":"text/plain", "$ref":"#/$defs/text"}),
        ] {
            assert!(!is_prose_schema(&schema));
        }
        assert!(!prose_field(
            &json!({"writeOnly":true,"properties":{"answer":{"type":"string","contentMediaType":"text/plain"}}}),
            "answer"
        ));
    }
}
