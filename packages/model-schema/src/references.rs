//! Bounded local-reference expansion for provider dialects that require inline schemas.
use crate::{SchemaPortabilityError, traversal};
use serde_json::Value;

/// Expand local JSON-pointer references without changing their validation constraints.
///
/// Annotations adjacent to a reference override target annotations. Semantic siblings
/// reject rather than being incorrectly merged. Literal annotation data is not traversed.
///
/// # Errors
/// Rejects unsupported dialects, recursive/external/unresolved references, semantic
/// reference siblings, and expansion beyond `max_bytes` or 128 levels of nesting.
pub fn inline_local_references(
    schema: &Value,
    max_bytes: usize,
) -> Result<Value, SchemaPortabilityError> {
    let draft = traversal::Draft::from_schema(schema)?;
    let mut budget = max_bytes;
    expand(schema, schema, draft, &mut budget, &mut Vec::new(), 0)
}

fn expand(
    root: &Value,
    schema: &Value,
    draft: traversal::Draft,
    budget: &mut usize,
    stack: &mut Vec<String>,
    depth: usize,
) -> Result<Value, SchemaPortabilityError> {
    if depth > 128 {
        return Err(SchemaPortabilityError::new(
            "",
            "schema expansion nesting limit exceeded",
        ));
    }
    let bytes = serde_json::to_vec(schema)
        .map_err(|_| SchemaPortabilityError::new("", "schema serialization failed"))?
        .len();
    *budget = budget
        .checked_sub(bytes)
        .ok_or_else(|| SchemaPortabilityError::new("", "schema expansion byte budget exceeded"))?;
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if stack.iter().any(|item| item == reference) {
            return Err(SchemaPortabilityError::new(
                "/$ref",
                "recursive schema cannot be inlined",
            ));
        }
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
            .ok_or_else(|| {
                SchemaPortabilityError::new(
                    "/$ref",
                    "reference must resolve to a local JSON pointer",
                )
            })?;
        let object = schema.as_object().expect("reference object");
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "$ref"
                    | "description"
                    | "title"
                    | "$comment"
                    | "default"
                    | "examples"
                    | "deprecated"
                    | "readOnly"
                    | "writeOnly"
            )
        }) {
            return Err(SchemaPortabilityError::new(
                "/$ref",
                "semantic reference siblings cannot be inlined",
            ));
        }
        stack.push(reference.to_owned());
        let mut result = expand(root, target, draft, budget, stack, depth + 1)?;
        stack.pop();
        if object.len() > 1 {
            let result = result.as_object_mut().ok_or_else(|| {
                SchemaPortabilityError::new("/$ref", "cannot annotate a boolean reference target")
            })?;
            for (key, value) in object {
                if key != "$ref" {
                    result.insert(key.clone(), value.clone());
                }
            }
        }
        return Ok(result);
    }
    let mut result = schema.clone();
    if let Some(object) = result.as_object_mut() {
        if object.contains_key("$id")
            || object.contains_key("$anchor")
            || object.contains_key("$dynamicRef")
            || object.contains_key("$dynamicAnchor")
        {
            return Err(SchemaPortabilityError::new(
                "",
                "schema resource identities cannot be inlined",
            ));
        }
        object.remove("$defs");
        object.remove("definitions");
    }
    for child in traversal::children(&result, draft) {
        let value = expand(
            root,
            result.pointer(&child).expect("schema child"),
            draft,
            budget,
            stack,
            depth + 1,
        )?;
        *result.pointer_mut(&child).expect("schema child") = value;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_nullable_references_preserve_constraints_and_literal_data() {
        let schema = json!({"type":"object","properties":{"check":{"$ref":"#/$defs/Check"}},"$defs":{"Check":{"type":"object","properties":{"execution":{"anyOf":[{"$ref":"#/$defs/Execution"},{"type":"null"}]}}},"Execution":{"type":"object","properties":{"argv":{"type":"array","minItems":1,"items":{"type":"string"}}}}},"examples":[{"$ref":"literal"}]});
        let result = inline_local_references(&schema, 100_000).unwrap();
        assert_eq!(
            result["properties"]["check"]["properties"]["execution"]["anyOf"][0],
            schema["$defs"]["Execution"]
        );
        assert_eq!(result["examples"], schema["examples"]);
        assert!(result.get("$defs").is_none());
        assert!(schema.get("$defs").is_some());
    }

    #[test]
    fn unsafe_or_unbounded_expansion_rejects() {
        for schema in [
            json!({"$ref":"#"}),
            json!({"$ref":"https://example.invalid/schema"}),
            json!({"$ref":"#/$defs/Missing"}),
            json!({"$ref":"#/$defs/S","maxLength":1,"$defs":{"S":{"type":"string"}}}),
        ] {
            assert!(inline_local_references(&schema, 100_000).is_err());
        }
        assert!(inline_local_references(&json!({"type":"string"}), 1).is_err());
    }
}
