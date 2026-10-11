//! Local, IO-free file-reference grammar. All offsets are UTF-8 byte offsets.

use std::ops::Range;

/// Maximum token size inspected on the input thread.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// An active reference and its decoded fuzzy query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveReference {
    /// Entire token, including `@` and optional quotes.
    pub range: Range<usize>,
    /// Decoded path prefix before the cursor.
    pub query: String,
}

/// Find a reference at the cursor without filesystem access.
///
/// References begin at start-of-input or after whitespace. Backslash escapes
/// the next scalar; double quotes delimit paths containing whitespace. Emails,
/// escaped triggers and references inside ordinary quoted prose do not activate.
#[must_use]
pub fn activate(text: &str, cursor: usize) -> Option<ActiveReference> {
    if !text.is_char_boundary(cursor) || text.len() > 262_144 {
        return None;
    }
    let mut tokens = text.char_indices().peekable();
    while let Some((start, first)) = tokens.next() {
        if first.is_whitespace() {
            continue;
        }
        let reference = first == '@';
        let mut quoted = first == '"';
        let mut escaped = first == '\\';
        let mut end = start + first.len_utf8();
        while let Some(&(offset, ch)) = tokens.peek() {
            if !quoted && !escaped && ch.is_whitespace() {
                break;
            }
            tokens.next();
            end = offset + ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = !quoted;
            }
        }
        if reference && (start + 1..=end).contains(&cursor) {
            if end - start > MAX_TOKEN_BYTES {
                return None;
            }
            let raw = &text[start + 1..end];
            // Quotes are permitted only as an enclosing pair, not midway through
            // an unquoted path. Partial opening quotes are valid while typing.
            let enclosed = raw.starts_with('"');
            let mut query = String::new();
            let mut escape = false;
            let mut closed = false;
            for (relative, ch) in raw.char_indices() {
                let offset = start + 1 + relative;
                if enclosed && relative == 0 {
                    continue;
                }
                if closed {
                    return None;
                }
                if escape {
                    if offset < cursor {
                        query.push(ch);
                    }
                    escape = false;
                } else if ch == '\\' {
                    escape = true;
                } else if ch == '"' {
                    if !enclosed {
                        return None;
                    }
                    closed = true;
                } else if offset < cursor {
                    query.push(ch);
                }
            }
            return Some(ActiveReference {
                range: start..end,
                query,
            });
        }
    }
    None
}

/// Encode exactly one path reference, round-trippable through the local grammar.
///
/// # Errors
/// Rejects empty, oversized or control-bearing paths. This function does not
/// authorize a path or read its contents.
pub fn encode_path(path: &str, directory: bool) -> Result<String, &'static str> {
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err("invalid display path");
    }
    let mut path = path.to_owned();
    if directory && !path.ends_with('/') {
        path.push('/');
    }
    let quoted = path
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\\' | '\''));
    let mut encoded = String::from("@");
    if quoted {
        encoded.push('"');
    }
    for ch in path.chars() {
        if matches!(ch, '"' | '\\') {
            encoded.push('\\');
        }
        encoded.push(ch);
    }
    if quoted {
        encoded.push('"');
    }
    if encoded.len() > MAX_TOKEN_BYTES {
        return Err("path token exceeds limit");
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_and_escaped_triggers() {
        for text in [
            "mail@example.com",
            "\\@file",
            "text@file",
            "\"@file\"",
            "(@file",
        ] {
            assert!(activate(text, text.len()).is_none(), "{text}");
        }
        for text in ["@", "say @src", "\n@src"] {
            assert!(activate(text, text.len()).is_some(), "{text}");
        }
    }

    #[test]
    fn quotes_escapes_unicode_round_trip() {
        for path in [
            "src/lib.rs",
            "a b/界.rs",
            "a\"b",
            "it's.rs",
            "a\\b",
            "👩‍💻/e\u{301}.rs",
        ] {
            let encoded = encode_path(path, false).unwrap();
            let token = activate(&encoded, encoded.len()).unwrap();
            assert_eq!(token.query, path);
            assert_eq!(token.range, 0..encoded.len());
        }
        let text = "@\"a b/界";
        assert_eq!(activate(text, text.len()).unwrap().query, "a b/界");
        assert_eq!(activate("@a\\ b", 5).unwrap().query, "a b");
    }

    #[test]
    fn middle_cursor_replaces_only_complete_active_token() {
        let text = "use @first @\"two words\" after @third";
        let cursor = text.find("words").unwrap();
        let active = activate(text, cursor).unwrap();
        assert_eq!(&text[active.range], "@\"two words\"");
        assert_eq!(active.query, "two ");
        assert!(activate("@界", 2).is_none());
    }

    #[test]
    fn bounded_and_directory_tokens() {
        assert!(activate(&format!("@{}", "x".repeat(MAX_TOKEN_BYTES)), 1).is_none());
        assert!(encode_path("bad\npath", false).is_err());
        let token = encode_path("two words", true).unwrap();
        assert_eq!(activate(&token, token.len()).unwrap().query, "two words/");
        assert!(activate("@a\"b", 4).is_none());
    }
}
