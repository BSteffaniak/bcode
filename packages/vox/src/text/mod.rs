//! Deterministic, delivery-independent processing of final transcripts.
//!
//! Custom phrases are case-sensitive, exact Unicode text (no normalization or
//! whitespace folding). At each Unicode word boundary the longest matching
//! source phrase wins; a custom phrase wins a tie with a spoken command. Output
//! is never scanned again. Spoken commands alone are ASCII-case-insensitive.
//! With spoken punctuation enabled, `literal <command>` emits that command's
//! original spelling without interpreting it or applying a custom replacement.
//! This reserved escape takes precedence over custom phrases at that position.
//! Unknown literal commands remain untouched.
//!
//! Control sanitization runs before matching and on the final output, including
//! replacement values. CRLF, CR and Unicode line separators become LF; tabs and
//! unsafe controls become spaces. LF is retained regardless of `Config::multiline`:
//! deciding whether a newline may reach an external application belongs to delivery.

use crate::config::Config;
use crate::contracts::{Error, Result, TextProcessor};
use unicode_segmentation::UnicodeSegmentation;

/// Construct an immutable text processor from the text settings in `config`.
///
/// Supported spoken commands are comma, period, full stop, question mark,
/// exclamation mark/point, colon, semicolon/semi colon, ellipsis, new line and
/// new paragraph. Repeated adjacent punctuation is suppressed, including ASR's
/// existing punctuation. Existing sentence-ending punctuation takes precedence
/// over a spoken sentence-ending mark. No punctuation is inferred otherwise.
///
/// # Errors
///
/// Returns [`Error::Invalid`] with a redacted message for an empty phrase, a
/// phrase with surrounding whitespace, or a phrase containing characters
/// sanitization would change. Replacement values may be empty and are sanitized
/// when emitted.
pub fn processor(config: &Config) -> Result<Box<dyn TextProcessor>> {
    let mut rules = Vec::with_capacity(config.replacements.len() + SPOKEN.len());
    for (phrase, replacement) in &config.replacements {
        if phrase.is_empty() || phrase.trim() != phrase || sanitize(phrase) != *phrase {
            return Err(Error::Invalid(
                "replacement phrases must be nonempty, trimmed and free of unsafe controls".into(),
            ));
        }
        rules.push(Rule {
            phrase: phrase.clone(),
            edit: Edit::Replace(replacement.clone()),
        });
    }
    if config.spoken_punctuation {
        rules.extend(SPOKEN.iter().map(|&(phrase, mark)| Rule {
            phrase: phrase.into(),
            edit: Edit::Punctuation(mark),
        }));
    }
    // Byte length orders overlapping exact prefixes correctly, including Unicode.
    // Lexical order then edit priority make the result independent of map iteration.
    rules.sort_by(|left, right| {
        right
            .phrase
            .len()
            .cmp(&left.phrase.len())
            .then_with(|| left.edit.priority().cmp(&right.edit.priority()))
            .then_with(|| left.phrase.cmp(&right.phrase))
    });
    Ok(Box::new(Processor {
        rules,
        spoken_punctuation: config.spoken_punctuation,
    }))
}

const SPOKEN: &[(&str, &str)] = &[
    ("exclamation point", "!"),
    ("exclamation mark", "!"),
    ("new paragraph", "\n\n"),
    ("question mark", "?"),
    ("semi colon", ";"),
    ("full stop", "."),
    ("semicolon", ";"),
    ("new line", "\n"),
    ("ellipsis", "…"),
    ("period", "."),
    ("comma", ","),
    ("colon", ":"),
];

struct Processor {
    rules: Vec<Rule>,
    spoken_punctuation: bool,
}

struct Rule {
    phrase: String,
    edit: Edit,
}

enum Edit {
    Replace(String),
    Punctuation(&'static str),
}

impl Edit {
    const fn priority(&self) -> u8 {
        match self {
            Self::Replace(_) => 0,
            Self::Punctuation(_) => 1,
        }
    }
}

impl TextProcessor for Processor {
    fn process(&self, text: &str) -> Result<String> {
        let source = sanitize(text);
        let boundaries = Boundaries::new(&source);
        let mut output = String::with_capacity(source.len());
        let mut offset = 0;
        while offset < source.len() {
            if self.spoken_punctuation
                && let Some((start, end)) = literal_command(&source, offset, &boundaries)
            {
                output.push_str(&source[start..end]);
                offset = end;
                continue;
            }
            if let Some(rule) = self.rules.iter().find(|rule| {
                matches_phrase(
                    &source,
                    offset,
                    &rule.phrase,
                    matches!(&rule.edit, Edit::Punctuation(_)),
                    &boundaries,
                )
            }) {
                offset += rule.phrase.len();
                match &rule.edit {
                    Edit::Replace(value) => output.push_str(value),
                    Edit::Punctuation(mark) => {
                        offset = emit_punctuation(&mut output, &source, offset, mark);
                    }
                }
            } else if let Some(character) = source[offset..].chars().next() {
                output.push(character);
                offset += character.len_utf8();
            }
        }
        Ok(sanitize(&output))
    }
}

// UAX #29 word boundaries protect apostrophes, combining marks and non-Latin
// words. Intersect with grapheme boundaries so emoji sequences remain indivisible.
struct Boundaries(Vec<bool>);

impl Boundaries {
    fn new(text: &str) -> Self {
        let mut boundaries = vec![false; text.len() + 1];
        for (offset, _) in text.split_word_bound_indices() {
            boundaries[offset] = true;
        }
        boundaries[text.len()] = true;
        let mut graphemes = vec![false; text.len() + 1];
        for (offset, _) in text.grapheme_indices(true) {
            graphemes[offset] = true;
        }
        graphemes[text.len()] = true;
        for (word, grapheme) in boundaries.iter_mut().zip(graphemes) {
            *word &= grapheme;
        }
        Self(boundaries)
    }
}

fn matches_phrase(
    source: &str,
    start: usize,
    phrase: &str,
    ignore_ascii_case: bool,
    boundaries: &Boundaries,
) -> bool {
    let Some(end) = start.checked_add(phrase.len()) else {
        return false;
    };
    if !boundaries.0.get(start).copied().unwrap_or(false)
        || !boundaries.0.get(end).copied().unwrap_or(false)
    {
        return false;
    }
    source.get(start..end).is_some_and(|candidate| {
        if ignore_ascii_case {
            candidate.eq_ignore_ascii_case(phrase)
        } else {
            candidate == phrase
        }
    })
}

fn literal_command(source: &str, offset: usize, boundaries: &Boundaries) -> Option<(usize, usize)> {
    if !matches_phrase(source, offset, "literal", true, boundaries) {
        return None;
    }
    let after_literal = offset + "literal".len();
    let remainder = &source[after_literal..];
    let command = remainder.trim_start_matches(horizontal_space);
    if command.len() == remainder.len() {
        return None;
    }
    let start = source.len() - command.len();
    SPOKEN
        .iter()
        .filter(|&&(phrase, _)| matches_phrase(source, start, phrase, true, boundaries))
        .max_by_key(|&&(phrase, _)| phrase.len())
        .map(|&(phrase, _)| (start, start + phrase.len()))
}

const fn horizontal_space(character: char) -> bool {
    character.is_whitespace() && character != '\n'
}

fn trim_horizontal_end(text: &mut String) {
    text.truncate(text.trim_end_matches(horizontal_space).len());
}

const fn sentence_mark(character: char) -> bool {
    matches!(character, '.' | '?' | '!' | '…')
}

fn duplicate_mark(character: char, mark: &str) -> bool {
    mark.starts_with(character) || (sentence_mark(character) && mark.chars().all(sentence_mark))
}

fn emit_punctuation(output: &mut String, source: &str, end: usize, mark: &str) -> usize {
    trim_horizontal_end(output);
    let remainder = &source[end..];
    let following = remainder.trim_start_matches(horizontal_space);
    let after_space = source.len() - following.len();
    if mark.starts_with('\n') {
        // ASR may already have rendered a requested line/paragraph break.
        let existing = following.chars().take_while(|&c| c == '\n').count();
        for _ in existing..mark.len() {
            output.push('\n');
        }
        return after_space;
    }
    let previous_is_duplicate = output
        .chars()
        .next_back()
        .is_some_and(|c| duplicate_mark(c, mark));
    if let Some(next) = following
        .chars()
        .next()
        .filter(|&c| duplicate_mark(c, mark))
    {
        // Preserve ASR's actual mark, but remove the space before it. When
        // both sides already have a mark, retain only the earlier one.
        return after_space
            + if previous_is_duplicate {
                next.len_utf8()
            } else {
                0
            };
    }
    if !previous_is_duplicate {
        output.push_str(mark);
    }
    end
}

fn sanitize(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                output.push('\n');
            }
            '\n' | '\u{2028}' | '\u{2029}' => output.push('\n'),
            // Keep ZWNJ/ZWJ, variation selectors and combining marks: they carry
            // real language/emoji content. Remove bidi overrides/isolates and
            // invisible separators rather than forwarding hidden directives.
            '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}' => output.push(' '),
            other if other.is_control() => output.push(' '),
            other => output.push(other),
        }
    }
    output
}

#[cfg(test)]
mod tests;
