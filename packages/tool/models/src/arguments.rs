//! Tool-argument normalization a tool may opt into.
//!
//! The host already removes `null`s that stand for omitted optional arguments. Empty strings are
//! different: `""` can be a real value, so only the tool can decide that a blank string means
//! "not given". Tools that treat blank optional strings as omitted apply
//! [`without_blank_values`] before parsing their arguments.
use serde_json::Value;

/// Tool arguments with blank values taken as not given.
///
/// A field whose value is an empty or whitespace-only string, or `null`, is dropped from every
/// object, including objects nested inside other objects and arrays. Array items themselves are
/// kept. Fields named in `blank_is_a_value` keep their strings exactly as given (for example an
/// empty replacement text that means "delete"). Required fields dropped this way are then refused
/// as missing by the tool's own parsing.
#[must_use]
pub fn without_blank_values(mut arguments: Value, blank_is_a_value: &[&str]) -> Value {
    strip(&mut arguments, blank_is_a_value);
    arguments
}

fn strip(value: &mut Value, blank_is_a_value: &[&str]) {
    match value {
        Value::Object(map) => {
            map.retain(|key, value| match value {
                Value::String(_) if blank_is_a_value.contains(&key.as_str()) => true,
                Value::String(text) => !text.trim().is_empty(),
                Value::Null => false,
                _ => true,
            });
            map.values_mut()
                .for_each(|value| strip(value, blank_is_a_value));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| strip(value, blank_is_a_value)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blank_values_are_dropped_from_every_object_and_nothing_else() {
        assert_eq!(
            without_blank_values(
                json!({"a":"","b":" ","c":null,"d":"x","e":0,"f":false,
                    "g":{"h":"","i":1},"j":[{"k":""},""]}),
                &[],
            ),
            json!({"d":"x","e":0,"f":false,"g":{"i":1},"j":[{},""]})
        );
        assert_eq!(without_blank_values(json!([1, ""]), &[]), json!([1, ""]));
    }

    #[test]
    fn declared_fields_keep_blank_strings_as_values() {
        assert_eq!(
            without_blank_values(
                json!({"replacement":"","edits":[{"replacement":" ","path":""}],"note":null}),
                &["replacement"],
            ),
            json!({"replacement":"","edits":[{"replacement":" "}]})
        );
    }
}
