//! Explicit bounded projections of already authenticated canonical output values.

use serde_json::{Value, json};

const MAX_POINTER_BYTES: usize = 256;
const MAX_RESPONSE_BYTES: usize = 3_000;

pub(super) fn selector(arguments: &Value) -> Result<Option<String>, String> {
    let Some(value) = arguments
        .get("output_pointer")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    let pointer = value
        .as_str()
        .ok_or("output_pointer must be a string or null")?;
    if arguments.get("output_only") != Some(&json!(true))
        || arguments
            .get("output_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || arguments.get("outputs_only") == Some(&json!(true))
        || arguments.get("delegation_only") == Some(&json!(true))
        || arguments
            .get("delegation_part")
            .is_some_and(|value| !value.is_null())
    {
        return Err("output_pointer requires output_only and an exact output_id".into());
    }
    if pointer.len() > MAX_POINTER_BYTES || (!pointer.is_empty() && !pointer.starts_with('/')) {
        return Err("invalid output_pointer".into());
    }
    let mut chars = pointer.chars();
    while let Some(character) = chars.next() {
        if character == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return Err("invalid output_pointer escape".into());
        }
    }
    Ok(Some(pointer.to_owned()))
}

pub(super) fn matches(descriptor: &Value, pointer: Option<&str>) -> bool {
    let normalized = pointer.map_or(Value::Null, |pointer| json!(pointer));
    descriptor.get("output_pointer").unwrap_or(&Value::Null) == &normalized
}

pub(super) fn render(
    context: &bcode_workflow::WorkflowExecutionContext,
    pointer: &str,
) -> Result<String, &'static str> {
    let mut output = context.output.as_ref().map_or(Value::Null, |output| {
        serde_json::to_value(output).expect("serialized canonical output")
    });
    let selected = output.get("value").and_then(|value| value.pointer(pointer));
    let selected_bytes = selected.map(|value| value.to_string().len());
    let selection = match selected {
        Some(value) => json!({
            "pointer": pointer, "status": "found", "complete": true,
            "selected_json_bytes": selected_bytes, "value": value,
        }),
        None if context.output.is_none() => json!({
            "pointer": pointer, "status": "output_unavailable", "complete": false,
        }),
        None => json!({"pointer": pointer, "status": "missing", "complete": true}),
    };
    if let Some(metadata) = output.as_object_mut() {
        metadata.remove("value");
        metadata.insert(
            "checksum_scope".into(),
            json!("whole_canonical_value_not_projection"),
        );
    }
    let mut response = json!({
        "run_id": context.run_id, "node_id": context.node_id,
        "activation_id": context.activation_id, "attempt": context.attempt,
        "revision": context.graph.revision, "output": output,
        "projection": selection,
    });
    let rendered = response.to_string();
    if rendered.len() <= MAX_RESPONSE_BYTES {
        return Ok(rendered);
    }
    response["projection"] = json!({
        "pointer": pointer, "status": "too_large", "complete": false,
        "selected_json_bytes": selected_bytes,
        "value_omitted": true, "response_byte_limit": MAX_RESPONSE_BYTES,
        "next_action": "For objects or arrays, retry the same output_id with a smaller child output_pointer. Scalar paging is unavailable; this selection remains incomplete.",
    });
    let rendered = response.to_string();
    if rendered.len() > MAX_RESPONSE_BYTES {
        return Err("output projection metadata exceeds response limit");
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests;
