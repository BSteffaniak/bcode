//! Lossless factoring of JSON Schema documentation embedded in tool descriptions.

use std::collections::BTreeMap;

use serde_json::{Value, json};

const MIN_PROPERTY_BYTES: usize = 256;
const EXPANSION_BUDGET: usize = 1024 * 1024;

/// Return compact documentation only when local-reference expansion restores the exact input.
pub(super) fn compact(schema: &Value) -> Value {
    let original = schema.to_string();
    if original.len() > EXPANSION_BUDGET || schema.get("$defs").is_some() {
        return schema.clone();
    }
    let mut occurrences = BTreeMap::<String, Vec<String>>::new();
    collect(schema, "", &mut occurrences);
    let mut candidate = schema.clone();
    let mut definitions = serde_json::Map::new();
    for (encoded, paths) in occurrences {
        if paths.len() < 2 {
            continue;
        }
        let Ok(property_schema) = serde_json::from_str::<Value>(&encoded) else {
            return schema.clone();
        };
        let name = format!("property{}", definitions.len());
        let reference = json!({"$ref":format!("#/$defs/{name}")});
        let mut replaced = 0;
        for path in paths {
            if let Some(value) = candidate.pointer_mut(&path)
                && *value == property_schema
            {
                *value = reference.clone();
                replaced += 1;
            }
        }
        if replaced > 0 {
            definitions.insert(name, property_schema);
        }
    }
    if definitions.is_empty() {
        return schema.clone();
    }
    candidate["$defs"] = Value::Object(definitions);
    if candidate.to_string().len() < original.len()
        && bcode_model_schema::inline_local_references(&candidate, EXPANSION_BUDGET)
            .is_ok_and(|expanded| expanded == *schema)
    {
        candidate
    } else {
        schema.clone()
    }
}

fn collect(schema: &Value, path: &str, occurrences: &mut BTreeMap<String, Vec<String>>) {
    // Traverse schema positions only. Descriptions, defaults and examples are literal data.
    for keyword in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(properties) = schema.get(keyword).and_then(Value::as_object) {
            for (name, value) in properties {
                let escaped = name.replace('~', "~0").replace('/', "~1");
                let child = format!("{path}/{keyword}/{escaped}");
                let encoded = value.to_string();
                if keyword == "properties" && encoded.len() >= MIN_PROPERTY_BYTES {
                    occurrences.entry(encoded).or_default().push(child.clone());
                }
                collect(value, &child, occurrences);
            }
        }
    }
    for keyword in [
        "items",
        "additionalProperties",
        "if",
        "then",
        "else",
        "not",
        "contains",
    ] {
        if let Some(value) = schema.get(keyword).filter(|value| value.is_object()) {
            collect(value, &format!("{path}/{keyword}"), occurrences);
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(values) = schema.get(keyword).and_then(Value::as_array) {
            for (index, value) in values.iter().enumerate() {
                collect(value, &format!("{path}/{keyword}/{index}"), occurrences);
            }
        }
    }
}

#[cfg(test)]
mod tests;
