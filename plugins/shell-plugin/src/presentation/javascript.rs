//! Optional compiled JavaScript/JSX display adapter; never execution or copy authority.
//!
//! One progressive policy inserts whitespace around recognized block delimiters and statement
//! terminators. Existing bytes (including whitespace) are never rewritten. Literals, comments,
//! JSX, and recovery islands are opaque. This is display beautification, not an ASI or semantic
//! equivalence guarantee. Missing syntax is never synthesized.

use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser};

use super::document::{
    AdapterInput, Budget, Document, Fragment, LanguageAdapter, Provenance, Role,
};

/// Parser-backed, stateless JavaScript display policy. Feature gating belongs to the caller.
#[derive(Clone, Copy, Debug, Default)]
pub struct JavaScriptAdapter;

impl LanguageAdapter for JavaScriptAdapter {
    fn present(&self, document: &mut Document, input: &AdapterInput, budget: &Budget) -> bool {
        let Some(source) = document
            .sources
            .get(input.source.0)
            .and_then(|source| source.text.get(input.range.clone()))
        else {
            return false;
        };
        let remaining = budget
            .max_fragments
            .saturating_sub(document.fragments.len());
        if source.len() > budget.max_source_bytes
            || input.depth >= budget.max_depth
            || remaining == 0
        {
            return false;
        }
        let Some(tokens) = parse_tokens(source, budget) else {
            return false;
        };
        let Some(fragments) = project(
            source,
            &tokens,
            input,
            remaining,
            budget.max_source_bytes.saturating_mul(4),
        ) else {
            return false;
        };
        document.fragments.extend(fragments);
        true
    }
}

#[derive(Debug)]
struct Token {
    range: Range<usize>,
    role: Role,
    opens: bool,
    closes: bool,
    ends_statement: bool,
}

fn parse_tokens(source: &str, budget: &Budget) -> Option<Vec<Token>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_javascript::LANGUAGE.into())
        .ok()?;
    let started = Instant::now();
    let mut checks = 0_usize;
    let mut progress = |_: &tree_sitter::ParseState| {
        checks += 1;
        if checks > 256 || started.elapsed() > Duration::from_millis(20) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = source.as_bytes();
    let mut read = |offset: usize, _: tree_sitter::Point| bytes.get(offset..).unwrap_or_default();
    let mut tree = parser.parse_with_options(
        &mut read,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )?;
    if tree.root_node().has_error() {
        // An unfinished enclosing function/block can become one ERROR island, hiding otherwise
        // complete statements. Supply parser-only block closures to recover that prior structure.
        // Counting opening bytes is an upper bound, not a second lexer: braces inside literals
        // may produce extra recovery errors, which remain opaque. No suffix byte is displayed.
        // Both parses share the same progress/time allowance; padding is at most source.len()+1.
        let closures = source.bytes().filter(|byte| *byte == b'{').count();
        if closures > budget.max_fragments {
            return None;
        }
        if closures != 0 {
            let recovery_len = source.len().checked_add(closures)?.checked_add(1)?;
            if recovery_len > budget.max_source_bytes {
                return None;
            }
            let mut recovery = String::with_capacity(recovery_len);
            recovery.push_str(source);
            recovery.push('\n');
            recovery.extend(std::iter::repeat_n('}', closures));
            let bytes = recovery.as_bytes();
            let mut read =
                |offset: usize, _: tree_sitter::Point| bytes.get(offset..).unwrap_or_default();
            tree = parser.parse_with_options(
                &mut read,
                None,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            )?;
        }
    }
    let mut cursor = tree.walk();
    // Iterative traversal needs no depth-limited Rust stack. The visit/fragment budgets
    // bound AST work independently of nested-language adapter admission.
    let mut visited = 0_usize;
    let mut tokens = Vec::new();
    loop {
        visited += 1;
        if visited > budget.max_fragments.saturating_mul(8) {
            return None;
        }
        let node = cursor.node();
        if !node.is_missing() && node.start_byte() < source.len() {
            if !opaque(node) && cursor.goto_first_child() {
                continue;
            }
            if node.end_byte() > node.start_byte() {
                let mut token = token(node);
                if token.range.end > source.len() {
                    token.range.end = source.len();
                    // A leaf extending into recovery text is uncertain original source, not a
                    // completed delimiter/statement just because padding satisfied the parser.
                    token.opens = false;
                    token.closes = false;
                    token.ends_statement = false;
                }
                tokens.push(token);
                if tokens.len() > budget.max_fragments {
                    return None;
                }
            }
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Some(tokens);
            }
        }
    }
}

fn opaque(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "string"
            | "template_string"
            | "regex"
            | "comment"
            | "hash_bang_line"
            | "jsx_element"
            | "jsx_self_closing_element"
    )
}

fn token(node: Node<'_>) -> Token {
    let kind = node.kind();
    let parent = node.parent().map_or("", |parent| parent.kind());
    let block = matches!(
        parent,
        "statement_block"
            | "class_body"
            | "switch_body"
            | "object"
            | "object_pattern"
            | "array"
            | "array_pattern"
    );
    let role = match kind {
        "string" | "template_string" | "regex" => Role::String,
        "comment" | "hash_bang_line" => Role::Comment,
        "if" | "else" | "for" | "while" | "do" | "switch" | "case" | "default" | "return"
        | "throw" | "break" | "continue" | "function" | "class" | "const" | "let" | "var"
        | "try" | "catch" | "finally" | "async" | "await" => Role::Keyword,
        "number" => Role::Number,
        "identifier"
        | "property_identifier"
        | "shorthand_property_identifier"
        | "shorthand_property_identifier_pattern" => {
            if node.parent().is_some_and(|parent| {
                (parent.kind() == "call_expression"
                    && parent.child_by_field_name("function") == Some(node))
                    || (matches!(
                        parent.kind(),
                        "function_declaration" | "function_expression" | "method_definition"
                    ) && parent.child_by_field_name("name") == Some(node))
            }) {
                Role::Function
            } else {
                Role::Identifier
            }
        }
        _ if node.is_error() => Role::Literal,
        _ if !node.is_named() => Role::Operator,
        _ => Role::Literal,
    };
    let certain = !node.has_error() && !node.is_missing() && !node.is_error();
    Token {
        range: node.byte_range(),
        role,
        opens: certain && block && matches!(kind, "{" | "["),
        closes: certain && block && matches!(kind, "}" | "]"),
        ends_statement: certain
            && ((kind == ","
                && matches!(
                    parent,
                    "object" | "object_pattern" | "array" | "array_pattern"
                ))
                || (kind == ";"
                    && !for_header_semicolon(node)
                    && matches!(
                        parent,
                        "expression_statement"
                            | "lexical_declaration"
                            | "variable_declaration"
                            | "return_statement"
                            | "throw_statement"
                            | "break_statement"
                            | "continue_statement"
                            | "empty_statement"
                    ))),
    }
}

// A for initializer is a declaration node too, but its semicolon separates header fields,
// not displayed statements. Distinguish the initializer from a single-statement loop body.
fn for_header_semicolon(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let Some(loop_node) = parent.parent() else {
        return false;
    };
    loop_node.kind() == "for_statement" && loop_node.child_by_field_name("body") != Some(parent)
}

fn project(
    source: &str,
    tokens: &[Token],
    input: &AdapterInput,
    max_fragments: usize,
    max_generated_bytes: usize,
) -> Option<Vec<Fragment>> {
    let mut fragments = Vec::new();
    let mut offset = 0;
    let mut indent = 0_usize;
    let mut previous_break = false;
    let mut generated_bytes = 0_usize;
    for token in tokens {
        if token.range.start < offset || source.get(token.range.clone()).is_none() {
            return None;
        }
        let gap = source.get(offset..token.range.start)?;
        // Whitespace is retained too: provenance covers every admitted source byte once.
        let gap_role = if gap.chars().all(char::is_whitespace) {
            Role::Whitespace
        } else {
            Role::Literal
        };
        append_source(
            &mut fragments,
            source,
            input,
            offset..token.range.start,
            gap_role,
        )?;
        if token.closes {
            indent = indent.saturating_sub(1);
        }
        if (previous_break || token.closes)
            && offset != 0
            && !gap.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
            && gap.chars().all(char::is_whitespace)
        {
            // Bound indentation amplification independently of extraction recursion depth.
            generated_bytes =
                generated_bytes.checked_add(indent.checked_mul(2)?.checked_add(1)?)?;
            if generated_bytes > max_generated_bytes {
                return None;
            }
            fragments.push(Fragment {
                text: format!("\n{}", "  ".repeat(indent)),
                role: Role::Whitespace,
                provenance: Provenance::Generated,
            });
        }
        append_source(
            &mut fragments,
            source,
            input,
            token.range.clone(),
            token.role,
        )?;
        offset = token.range.end;
        if token.opens {
            indent += 1;
        }
        previous_break = token.opens || token.ends_statement;
        if fragments.len() > max_fragments {
            return None;
        }
    }
    append_source(
        &mut fragments,
        source,
        input,
        offset..source.len(),
        Role::Literal,
    )?;
    (fragments.len() <= max_fragments).then_some(fragments)
}

fn append_source(
    fragments: &mut Vec<Fragment>,
    source: &str,
    input: &AdapterInput,
    range: Range<usize>,
    role: Role,
) -> Option<()> {
    let text = source.get(range.clone())?;
    if !text.is_empty() {
        fragments.push(Fragment {
            text: text.to_owned(),
            role,
            provenance: Provenance::Source {
                id: input.source,
                range: input.range.start + range.start..input.range.start + range.end,
            },
        });
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::super::document::SourceId;
    use super::*;

    fn present(source: &str) -> Document {
        let mut document = Document::original(source);
        assert!(JavaScriptAdapter.present(
            &mut document,
            &AdapterInput {
                source: SourceId(0),
                range: 0..source.len(),
                depth: 0,
            },
            &Budget::default()
        ));
        document
    }

    fn assert_coverage(document: &Document, source: &str) {
        let mut end = 0;
        for fragment in &document.fragments {
            if let Provenance::Source { id, range } = &fragment.provenance {
                assert_eq!(*id, SourceId(0));
                assert_eq!(range.start, end);
                assert_eq!(fragment.text, source[range.clone()]);
                end = range.end;
            } else {
                assert!(fragment.text.chars().all(char::is_whitespace));
            }
        }
        assert_eq!(end, source.len());
        assert_eq!(document.execution_text(), source);
    }

    #[test]
    fn formats_blocks_and_preserves_source_coverage() {
        let source = "function f(){const x=1;return x;}";
        let document = present(source);
        assert_eq!(
            document.display_text(),
            "function f(){\n  const x=1;\n  return x;\n}"
        );
        assert_coverage(&document, source);
    }

    #[test]
    fn formats_nested_object_and_array_structure() {
        // `{c:3}` is JavaScript object syntax, deliberately not a Rust formatting field.
        #[allow(clippy::literal_string_with_formatting_args)]
        let source = "const x={a:[1,2],b:{c:3}};";
        let document = present(source);
        assert_eq!(
            document.display_text(),
            "const x={\n  a:[\n    1,\n    2\n  ],\n  b:{\n    c:3\n  }\n};"
        );
        assert_coverage(&document, source);
    }

    #[test]
    fn literals_comments_jsx_and_unicode_are_opaque() {
        for literal in [
            r"`😀 ${x + y}`",
            r"/a{1,2};\/x/g",
            r#""a;{界}""#,
            "\"e\u{301}\"",
            "/* { ; 😀 } */",
            "<div>😀;{value}</div>",
        ] {
            let source = if literal.starts_with("/*") {
                format!("function f(){{{literal}const x=1;}}")
            } else {
                format!("function f(){{const x={literal};}}")
            };
            let document = present(&source);
            assert!(
                document
                    .fragments
                    .iter()
                    .any(|fragment| fragment.text == literal)
            );
            assert_coverage(&document, &source);
        }
    }

    #[test]
    fn recovered_statement_keeps_identifier_function_and_number_roles() {
        let source = "const value=42;print(value); let result=(界";
        for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
            let prefix = &source[..end];
            let document = present(prefix);
            assert_coverage(&document, prefix);
            if end >= "const value=42".len() {
                assert!(
                    document
                        .fragments
                        .iter()
                        .any(|fragment| fragment.text == "42" && fragment.role == Role::Number),
                    "{prefix}"
                );
            }
            if end >= "const value=42;print(value)".len() {
                assert!(document.fragments.iter().any(|fragment| fragment.text == "print" && fragment.role == Role::Function), "{prefix}");
            }
        }
    }

    #[test]
    fn all_prefixes_use_the_same_policy_without_invented_syntax() {
        let source = "function f(){const x=`界 ${a}`;return /x{2}/g;}";
        for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
            let prefix = &source[..end];
            assert_coverage(&present(prefix), prefix);
        }
        let partial = present("function f(){const x=1;").display_text();
        let complete = present("function f(){const x=1;}").display_text();
        assert!(
            complete.starts_with(&partial),
            "partial={partial:?}, complete={complete:?}"
        );
    }

    #[test]
    fn recovered_enclosing_blocks_keep_prior_structure_progressive() {
        for (partial, suffix, expected) in [
            (
                "function f(){const x=1;",
                "}",
                "function f(){\n  const x=1;",
            ),
            ("if(x){foo();bar();", "}", "if(x){\n  foo();\n  bar();"),
            (
                "function f(){if(x){foo();",
                "}}",
                "function f(){\n  if(x){\n    foo();",
            ),
        ] {
            let before = present(partial);
            let completed_source = format!("{partial}{suffix}");
            let after = present(&completed_source);
            assert_eq!(before.display_text(), expected);
            assert!(
                after.display_text().starts_with(expected),
                "before={:?}, after={:?}",
                before.display_text(),
                after.display_text()
            );
            assert_coverage(&before, partial);
            assert_coverage(&after, &completed_source);
        }
    }

    #[test]
    fn retains_line_terminators_and_does_not_split_for_header() {
        let source = "function f(){return\n{x:1};for(let i=0;i<2;i++){f();}}";
        let document = present(source);
        let display = document.display_text();
        assert!(display.contains("return\n"), "{display:?}");
        assert_unsplit_header(&document, source, "for(let i=0;i<2;i++)");
        assert_coverage(&document, source);
    }

    fn assert_unsplit_header(document: &Document, source: &str, header: &str) {
        let start = source.find(header).unwrap();
        let end = start + header.len();
        let mut offset = 0;
        for fragment in &document.fragments {
            match &fragment.provenance {
                Provenance::Source { range, .. } => offset = range.end,
                Provenance::Generated => assert!(
                    !(start < offset && offset < end && fragment.text.contains('\n')),
                    "header split at {offset}: {:?}",
                    document.display_text(),
                ),
            }
        }
    }

    #[test]
    fn for_initializers_do_not_break_but_single_statement_bodies_do() {
        for header in [
            "for(let i=0;i<2;i++)",
            "for(var i=0;i<2;i++)",
            "for(i=0;i<2;i++)",
            "for(;;)",
        ] {
            let source = format!("{header}f();g();");
            let document = present(&source);
            assert_unsplit_header(&document, &source, header);
            assert!(
                document.display_text().contains("f();\ng();"),
                "{:?}",
                document.display_text()
            );
            assert_coverage(&document, &source);
        }
    }

    #[test]
    fn incomplete_literals_never_gain_whitespace_inside_their_source() {
        for tail in [
            "`界 ${a", "`a;{\\`", "'a;{\\", "\"a;{\\", "/a{2};[", "/a\\/",
        ] {
            let source = format!("function f(){{const x={tail}");
            let start = source.len() - tail.len();
            let document = present(&source);
            let mut offset = 0;
            for fragment in &document.fragments {
                match &fragment.provenance {
                    Provenance::Source { range, .. } => offset = range.end,
                    Provenance::Generated => {
                        assert!(offset <= start, "{:?}", document.display_text());
                    }
                }
            }
            assert_coverage(&document, &source);
        }
    }

    #[test]
    fn restricted_production_and_comment_line_terminators_stay_source_backed() {
        for boundary in [
            "return\r\nvalue",
            "return\u{2028}value",
            "return\u{2029}value",
            "throw\nvalue",
            "value\n++other",
            "return/*\n*/value",
            "return// note\nvalue",
        ] {
            let source = format!("function f(){{{boundary};}}");
            let document = present(&source);
            assert_coverage(&document, &source);
            // Original source fragments are in order; generated breaks never replace source
            // line terminators or the opaque comments which carry them.
            let retained: String = document
                .fragments
                .iter()
                .filter(|fragment| matches!(fragment.provenance, Provenance::Source { .. }))
                .map(|fragment| fragment.text.as_str())
                .collect();
            assert!(retained.contains(boundary));
        }
    }

    #[test]
    fn ordinary_compounds_format_at_nested_adapter_depth_one_limit_four() {
        let budget = Budget {
            max_depth: 4,
            ..Budget::default()
        };
        for source in [
            "if(x){foo();bar()}",
            "function f(){if(x){while(y){for(let i=0;i<2;i++){foo();bar();}}}}",
        ] {
            let mut document = Document::original(source);
            let input = AdapterInput {
                source: SourceId(0),
                range: 0..source.len(),
                depth: 1,
            };
            assert!(JavaScriptAdapter.present(&mut document, &input, &budget));
            assert!(
                document.display_text().contains("foo();\n"),
                "{:?}",
                document.display_text()
            );
            assert_coverage(&document, source);
            let mut rejected = Document::original(source);
            assert!(!JavaScriptAdapter.present(
                &mut rejected,
                &AdapterInput { depth: 4, ..input },
                &budget
            ));
            assert!(rejected.fragments.is_empty());
        }
    }

    #[test]
    fn indentation_amplification_rejection_is_atomic() {
        let source = format!("{}x();{}", "if(x){".repeat(20), "}".repeat(20));
        let budget = Budget {
            max_source_bytes: source.len(),
            max_depth: 4,
            ..Budget::default()
        };
        let mut document = Document::original(&source);
        assert!(!JavaScriptAdapter.present(
            &mut document,
            &AdapterInput {
                source: SourceId(0),
                range: 0..source.len(),
                depth: 1,
            },
            &budget
        ));
        assert!(document.fragments.is_empty());
    }

    #[test]
    fn rejected_work_is_atomic() {
        let source = "function f(){x();}";
        for budget in [
            Budget {
                max_source_bytes: 1,
                ..Budget::default()
            },
            Budget {
                max_fragments: 1,
                ..Budget::default()
            },
            Budget {
                max_depth: 1,
                ..Budget::default()
            },
        ] {
            let mut document = Document::original(source);
            assert!(document.push_generated(" "));
            let before = document.clone();
            assert!(!JavaScriptAdapter.present(
                &mut document,
                &AdapterInput {
                    source: SourceId(0),
                    range: 0..source.len(),
                    depth: 1,
                },
                &budget
            ));
            assert_eq!(document, before);
        }
    }

    #[test]
    fn decoded_slice_ranges_are_relative_to_retained_source() {
        let mut document = Document::original("encoded");
        let source = document
            .add_decoded("xxfunction f(){x();}yy".to_owned(), SourceId(0), 0..7)
            .unwrap();
        let end = document.sources[source.0].text.len() - 2;
        assert!(JavaScriptAdapter.present(
            &mut document,
            &AdapterInput {
                source,
                range: 2..end,
                depth: 0,
            },
            &Budget::default()
        ));
        assert!(
            document
                .fragments
                .iter()
                .all(|fragment| match &fragment.provenance {
                    Provenance::Generated => true,
                    Provenance::Source { id, range } =>
                        *id == source && range.start >= 2 && range.end <= end,
                })
        );
        assert_eq!(document.execution_text(), "encoded");
    }
}
