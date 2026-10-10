//! Static embedded-source extraction from shell-engine structural ranges.
//!
//! This module never parses shell command boundaries, runs an interpreter, or
//! infers a language from a heredoc delimiter. The shell engine supplies words
//! for one simple command and its effective standard-input heredoc. Uncertain
//! decoding is presentation-only and must not be treated as executable source.

use std::ops::Range;

/// The interpreter selected by a statically known command word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Python,
    JavaScript,
    Shell,
}

/// Structural facts supplied by the shell engine, not by delimiter heuristics.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// Command name followed by argument words; excludes assignments/redirections.
    pub words: Vec<Range<usize>>,
    /// Only the effective fd-0 heredoc, after resolving redirection ordering.
    /// Use `None` when input association is uncertain or redirected elsewhere.
    pub stdin: Option<Heredoc>,
}

/// An interpreter-associated heredoc, with the delimiter already excluded.
#[derive(Clone, Debug)]
pub struct Heredoc {
    pub body: Range<usize>,
    /// Any quoted delimiter component suppresses body expansion.
    pub quoted: bool,
    /// `<<-` removes leading tabs (not spaces) from every body line.
    pub strip_tabs: bool,
    pub complete: bool,
}

/// One decoded Unicode scalar and the exact bytes responsible for it.
///
/// For an escape, `original` includes the backslash. Quotes and removed
/// continuations/tabs have no decoded bytes and therefore no mapping entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Correspondence {
    pub decoded: Range<usize>,
    pub original: Range<usize>,
}

/// Owned, renderer-neutral extraction, independent of the document contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extraction {
    pub language: Language,
    pub original: Range<usize>,
    pub text: String,
    pub correspondence: Vec<Correspondence>,
    /// False for unterminated syntax, incomplete heredocs or opaque expansions.
    pub complete: bool,
    /// Expansions/unsupported shell syntax remain literal, never evaluated.
    /// Consumers must not interpret uncertain text as statically decoded code.
    pub uncertain: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Extract a literal eval argument or the effective interpreter stdin heredoc.
///
/// Invalid ranges, dynamic interpreter/option words, unsupported invocation
/// shapes, and a source larger than `max_bytes` fail closed with `None`.
/// Work and output allocation are linear in inspected bytes, bounded by the
/// caller's budget. No project configuration or subprocess is consulted.
pub fn extract(source: &str, invocation: &Invocation, max_bytes: usize) -> Option<Extraction> {
    // Bound cumulative argument inspection as well as embedded-source size.
    let mut remaining = max_bytes;
    let mut word = |range: &Range<usize>| {
        let raw = source.get(range.clone())?;
        remaining = remaining.checked_sub(raw.len().max(1))?;
        Some(decode_word(raw, range.start))
    };
    let command = word(invocation.words.first()?)?;
    if command.uncertain || !command.complete {
        return None;
    }
    let name = command.text.rsplit('/').next()?;
    let language = match name {
        "python" | "python3" => Language::Python,
        "node" => Language::JavaScript,
        "sh" | "bash" => Language::Shell,
        _ => return None,
    };
    let mut args = invocation.words.iter().skip(1);
    let mut shell_stdin = false;
    while let Some(range) = args.next() {
        let argument = word(range)?;
        if argument.uncertain {
            return None;
        }
        let eval = match language {
            Language::Python | Language::Shell => argument.text == "-c",
            Language::JavaScript => matches!(argument.text.as_str(), "-e" | "--eval"),
        };
        if eval && argument.complete {
            let range = args.next()?;
            let mut extracted = word(range)?;
            extracted.language = language;
            return Some(extracted);
        }
        if language == Language::JavaScript
            && let Some(code) = argument.text.strip_prefix("--eval=")
        {
            let prefix = argument.text.len() - code.len();
            let text = code.to_owned();
            let mut extracted = argument;
            extracted.language = language;
            extracted.text = text;
            extracted.correspondence.retain_mut(|mapping| {
                if mapping.decoded.start < prefix {
                    return false;
                }
                mapping.decoded.start -= prefix;
                mapping.decoded.end -= prefix;
                true
            });
            return Some(extracted);
        }
        if !argument.complete {
            return None;
        }
        // Only explicitly understood, operand-free options may precede source.
        let harmless = match language {
            Language::Python => matches!(
                argument.text.as_str(),
                "-u" | "-B" | "-E" | "-I" | "-S" | "-s"
            ),
            Language::Shell => matches!(argument.text.as_str(), "-s" | "-e" | "-u" | "-x"),
            Language::JavaScript => false,
        };
        if argument.text == "-" {
            // Remaining arguments are script arguments, not interpreter options.
            break;
        }
        if language == Language::Shell && argument.text == "-s" {
            shell_stdin = true;
            continue;
        }
        if shell_stdin && (argument.text == "--" || !argument.text.starts_with('-')) {
            // Everything after `--` or the first positional word is a script argument.
            break;
        }
        if !harmless {
            return None;
        }
    }
    let heredoc = invocation.stdin.as_ref()?;
    let raw = source.get(heredoc.body.clone())?;
    remaining.checked_sub(raw.len())?;
    let mut extracted = decode_heredoc(raw, heredoc);
    extracted.language = language;
    Some(extracted)
}

const fn empty(original: Range<usize>) -> Extraction {
    Extraction {
        language: Language::Shell,
        original,
        text: String::new(),
        correspondence: Vec::new(),
        complete: true,
        uncertain: false,
    }
}

fn append(output: &mut Extraction, character: char, original: Range<usize>) {
    let start = output.text.len();
    output.text.push(character);
    output.correspondence.push(Correspondence {
        decoded: start..output.text.len(),
        original,
    });
}

fn raw_word(raw: &str, start: usize) -> Extraction {
    let mut output = empty(start..start + raw.len());
    output.uncertain = true;
    // Expansion syntax is deliberately opaque, so completeness is unverified.
    output.complete = false;
    for (offset, character) in raw.char_indices() {
        append(
            &mut output,
            character,
            start + offset..start + offset + character.len_utf8(),
        );
    }
    output
}

fn decode_word(raw: &str, start: usize) -> Extraction {
    let mut output = empty(start..start + raw.len());
    let mut quote = Quote::None;
    let mut characters = raw.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        let position = start + offset;
        match (quote, character) {
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::None | Quote::Double, '$' | '`')
            | (Quote::None, '*' | '?' | '[' | '~' | '{' | '}') => {
                // Do not try to delimit or evaluate shell expansions.
                return raw_word(raw, start);
            }
            (Quote::None | Quote::Double, '\\') => {
                let Some((next_offset, next)) = characters.peek().copied() else {
                    output.complete = false;
                    append(&mut output, character, position..position + 1);
                    break;
                };
                let escaped = quote == Quote::None || matches!(next, '$' | '`' | '"' | '\\' | '\n');
                if escaped {
                    characters.next();
                    if next != '\n' {
                        append(
                            &mut output,
                            next,
                            position..start + next_offset + next.len_utf8(),
                        );
                    }
                } else {
                    append(&mut output, character, position..position + 1);
                }
            }
            _ => append(
                &mut output,
                character,
                position..position + character.len_utf8(),
            ),
        }
    }
    output.complete &= quote == Quote::None;
    output
}

fn decode_heredoc(raw: &str, heredoc: &Heredoc) -> Extraction {
    let mut output = empty(heredoc.body.clone());
    output.complete = heredoc.complete;
    let mut characters = raw.char_indices().peekable();
    let mut line_start = true;
    while let Some((offset, character)) = characters.next() {
        let position = heredoc.body.start + offset;
        if line_start && heredoc.strip_tabs && character == '\t' {
            continue;
        }
        line_start = character == '\n';
        if !heredoc.quoted {
            if matches!(character, '$' | '`') {
                output.uncertain = true;
            }
            if character == '\\' {
                if let Some((next_offset, next)) = characters.peek().copied() {
                    if matches!(next, '$' | '`' | '\\' | '\n') {
                        characters.next();
                        if next == '\n' {
                            // Tab stripping occurs on physical lines, including
                            // lines joined by backslash-newline removal.
                            line_start = true;
                        } else {
                            append(
                                &mut output,
                                next,
                                position..heredoc.body.start + next_offset + next.len_utf8(),
                            );
                        }
                        continue;
                    }
                } else if !heredoc.complete {
                    output.complete = false;
                }
            }
        }
        append(
            &mut output,
            character,
            position..position + character.len_utf8(),
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(parts: &[&str]) -> (String, Invocation) {
        let mut source = String::new();
        let mut words = Vec::new();
        for part in parts {
            if !source.is_empty() {
                source.push(' ');
            }
            let start = source.len();
            source.push_str(part);
            words.push(start..source.len());
        }
        (source, Invocation { words, stdin: None })
    }

    fn eval(parts: &[&str]) -> Extraction {
        let (source, invocation) = invocation(parts);
        extract(&source, &invocation, 4096).unwrap()
    }

    #[test]
    fn recognizes_interpreters_and_literal_concatenation() {
        for name in ["python", "python3", "/usr/bin/python3", "sh", "bash"] {
            let extracted = eval(&[name, "-c", "'a'\"b\"c\\ d"]);
            assert_eq!(extracted.text, "abc d");
            assert!(extracted.complete);
            assert!(!extracted.uncertain);
        }
        assert_eq!(
            eval(&["node", "-e", "'console.log(1)'"]).language,
            Language::JavaScript
        );
        assert_eq!(eval(&["node", "--eval", "'x'"]).text, "x");
        assert_eq!(eval(&["node", "'--eval=x y'"]).text, "x y");
        assert!(!eval(&["node", "--eval='unfinished"]).complete);
        assert_eq!(eval(&["bash", "-s", "-c", "'echo hi'"]).text, "echo hi");
    }

    #[test]
    fn maps_unicode_escapes_and_removed_syntax_to_original_bytes() {
        let (source, invocation) = invocation(&["python3", "-c", "'界'\\ 🦀\"\\q\"\\\nx"]);
        let extracted = extract(&source, &invocation, 4096).unwrap();
        assert_eq!(extracted.text, "界 🦀\\qx");
        for mapping in &extracted.correspondence {
            let decoded = &extracted.text[mapping.decoded.clone()];
            let original = &source[mapping.original.clone()];
            assert!(original == decoded || original.strip_prefix('\\') == Some(decoded));
        }
        let space = &extracted.correspondence[1];
        assert_eq!(&source[space.original.clone()], "\\ ");
    }

    #[test]
    fn incomplete_literals_are_useful_but_expansions_are_not_decoded() {
        let fragment = eval(&["python3", "-c", "'print(界)"]);
        assert_eq!(fragment.text, "print(界)");
        assert!(!fragment.complete);
        for raw in [
            "\"$HOME\"",
            "$(touch bad)",
            "`cmd`",
            "$'ansi'",
            "x*",
            "~user",
        ] {
            let extracted = eval(&["sh", "-c", raw]);
            assert!(extracted.uncertain, "{raw}");
            assert_eq!(extracted.text, raw);
        }
        assert!(!eval(&["sh", "-c", "x\\"]).complete);
        assert!(!eval(&["sh", "-c", "'${literal}'"]).uncertain);
        assert!(!eval(&["sh", "-c", "\"\\$literal\""]).uncertain);
    }

    fn heredoc(command: &[&str], body: &str, quoted: bool, strip_tabs: bool) -> Extraction {
        let (mut source, mut invocation) = invocation(command);
        source.push('\n');
        let start = source.len();
        source.push_str(body);
        invocation.stdin = Some(Heredoc {
            body: start..source.len(),
            quoted,
            strip_tabs,
            complete: false,
        });
        extract(&source, &invocation, 4096).unwrap()
    }

    #[test]
    fn heredocs_are_associated_with_interpreter_not_delimiter_name() {
        assert_eq!(
            heredoc(&["python3", "-"], "print(1)\n", true, false).language,
            Language::Python
        );
        assert_eq!(
            heredoc(&["node"], "console.log(1)\n", true, false).language,
            Language::JavaScript
        );
        assert_eq!(
            heredoc(&["bash", "-s", "--", "arg"], "echo hi\n", true, false).language,
            Language::Shell
        );
        for parts in [
            &["cat"][..],
            &["python3", "script.py"],
            &["node", "file.js"],
            &["sh", "file.sh"],
        ] {
            let (source, invocation) = invocation(parts);
            assert!(extract(&source, &invocation, 4096).is_none());
        }
    }

    #[test]
    fn heredoc_quoting_tabs_and_expansion_uncertainty() {
        let quoted = heredoc(&["python3"], "\t\t$literal\\\n\t  界\n", true, true);
        assert_eq!(quoted.text, "$literal\\\n  界\n");
        assert!(!quoted.uncertain);
        assert!(!quoted.complete);
        let unquoted = heredoc(&["sh"], "\t\\$literal\\\n\t界\\q\n", false, true);
        assert_eq!(unquoted.text, "$literal界\\q\n");
        assert!(!unquoted.uncertain);
        assert!(heredoc(&["node"], "$value\n", false, false).uncertain);
        assert!(heredoc(&["sh"], "`cmd`\n", false, false).uncertain);
    }

    #[test]
    fn invalid_dynamic_and_over_budget_inputs_fail_closed() {
        for parts in [
            &["$PY", "-c", "'x'"][..],
            &["python3", "$FLAG", "'x'"],
            &["node", "--require", "module", "-e", "'x'"],
        ] {
            let (source, invocation) = invocation(parts);
            assert!(extract(&source, &invocation, 4096).is_none());
        }
        let (source, mut invocation) = invocation(&["python3", "-c", "'界'"]);
        assert!(extract(&source, &invocation, 2).is_none());
        invocation.words[2] = source.len() - 3..source.len() - 2;
        assert!(extract(&source, &invocation, 4096).is_none());
    }
}
