//! Structural decoding of top-level string fields in streamed JSON objects.

/// Decode only complete JSON string units. Incomplete escapes remain invisible until complete.
pub fn string_field(input: &str, wanted: &str) -> Option<String> {
    let mut rest = input.trim_start().strip_prefix('{')?;
    loop {
        rest = rest.trim_start();
        let (key, consumed, complete) = string_prefix(rest)?;
        if !complete {
            return None;
        }
        rest = rest
            .get(consumed..)?
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        if key == wanted {
            return string_prefix(rest).map(|(value, _, _)| value);
        }
        // Serde owns nested values, escaped keys, booleans and numbers. Never search inside values.
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
        values.next()?.ok()?;
        rest = rest
            .get(values.byte_offset()..)?
            .trim_start()
            .strip_prefix(',')?;
    }
}

/// Read a completed top-level boolean without requiring the rest of the object.
pub fn bool_field(input: &str, wanted: &str) -> Option<bool> {
    let mut rest = input.trim_start().strip_prefix('{')?;
    loop {
        rest = rest.trim_start();
        let (key, consumed, complete) = string_prefix(rest)?;
        if !complete {
            return None;
        }
        rest = rest
            .get(consumed..)?
            .trim_start()
            .strip_prefix(':')?
            .trim_start();
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
        let value = values.next()?.ok()?;
        if key == wanted {
            return value.as_bool();
        }
        rest = rest
            .get(values.byte_offset()..)?
            .trim_start()
            .strip_prefix(',')?;
    }
}

fn string_prefix(input: &str) -> Option<(String, usize, bool)> {
    let encoded = input.strip_prefix('"')?;
    let mut end = 0;
    let mut units = encoded.char_indices().peekable();
    while let Some((offset, character)) = units.next() {
        match character {
            '"' => {
                let value = serde_json::from_str(&input[..offset + 2]).ok()?;
                return Some((value, offset + 2, true));
            }
            '\\' => {
                let Some((_, escape)) = units.next() else {
                    break;
                };
                match escape {
                    '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {}
                    'u' => {
                        let Some(high) = hex_unit(&mut units) else {
                            break;
                        };
                        if (0xd800..=0xdbff).contains(&high) {
                            // A surrogate pair is one indivisible decoded unit.
                            if !matches!(units.next(), Some((_, '\\')))
                                || !matches!(units.next(), Some((_, 'u')))
                            {
                                break;
                            }
                            let Some(low) = hex_unit(&mut units) else {
                                break;
                            };
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return None;
                            }
                        } else if (0xdc00..=0xdfff).contains(&high) {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            character if character < ' ' => return None,
            _ => {}
        }
        end = units.peek().map_or(encoded.len(), |(offset, _)| *offset);
    }
    let value = serde_json::from_str(&format!("\"{}\"", &encoded[..end])).ok()?;
    Some((value, input.len(), false))
}

fn hex_unit(units: &mut impl Iterator<Item = (usize, char)>) -> Option<u16> {
    let mut value = 0_u16;
    for _ in 0..4 {
        value = value.checked_mul(16)? + u16::try_from(units.next()?.1.to_digit(16)?).ok()?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_strings_only_expose_decoded_prefixes() {
        let json = r#"{"ignored":{"command":"wrong"},"command":"a  \"b\"\n\t\uD83D\uDE00界","cwd":"/tmp"}"#;
        let expected = "a  \"b\"\n\t😀界";
        for end in (0..=json.len()).filter(|end| json.is_char_boundary(*end)) {
            if let Some(value) = string_field(&json[..end], "command") {
                assert!(expected.starts_with(&value), "{end}: {value:?}");
            }
        }
        assert_eq!(string_field(json, "command").as_deref(), Some(expected));
    }

    #[test]
    fn does_not_find_keys_in_values_or_decode_invalid_escapes() {
        assert_eq!(
            string_field(r#"{"other":"command","x":"value"}"#, "command"),
            None
        );
        assert_eq!(string_field(r#"{"command":"bad\q"}"#, "command"), None);
        assert_eq!(
            string_field(r#"{"command":"ok\u12"#, "command").as_deref(),
            Some("ok")
        );
        assert_eq!(
            string_field(r#"{"command":"ok\"#, "command").as_deref(),
            Some("ok")
        );
    }
}
