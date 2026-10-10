//! Conservative Python presentation over decoded source; never an execution formatter.
//!
//! Semantic indentation, physical newlines, strings and comments retain exact source bytes.
//! Proven horizontal token gaps receive Bcode-owned spacing, and complete module-level simple
//! statements can gain generated newlines after semicolons. Recovery regions stay literal.

use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser};

use super::document::{
    AdapterInput, Budget, Document, Fragment, LanguageAdapter, Provenance, Role,
};

/// Optional compiled Python grammar adapter, independent of shell execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonAdapter;

struct Token<'tree> {
    node: Node<'tree>,
    range: Range<usize>,
    role: Role,
    certain: bool,
}

impl LanguageAdapter for PythonAdapter {
    fn present(&self, document: &mut Document, input: &AdapterInput, budget: &Budget) -> bool {
        let Some(source) = document
            .sources
            .get(input.source.0)
            .and_then(|source| source.text.get(input.range.clone()))
        else {
            return false;
        };
        if source.len() > budget.max_source_bytes
            || source.len() > u32::MAX as usize
            || input.depth >= budget.max_depth
        {
            return false;
        }
        let Some(tree) = parse(source) else {
            return false;
        };
        let Some(tokens) = collect_statements(tree.root_node(), source.len(), budget.max_fragments)
        else {
            return false;
        };
        let mut fragments = Vec::new();
        let mut offset = 0;
        let mut previous = None;
        for token in &tokens {
            if token.range.start < offset || token.range.end > source.len() {
                return false;
            }
            let gap = offset..token.range.start;
            let Some(text) = source.get(gap.clone()) else {
                return false;
            };
            let spacing = previous.and_then(|left| spacing(left, token, text, source));
            if previous.is_some_and(|left: &Token<'_>| {
                left.certain
                    && token.certain
                    && token.role != Role::Comment
                    && source.get(left.range.clone()) == Some(";")
                    && left
                        .node
                        .parent()
                        .is_some_and(|parent| parent.kind() == "module")
                    && text.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
            }) {
                // Only module-level simple statements can be split without inventing a suite
                // indentation level. Keep the semicolon source-backed; the newline is generated.
                fragments.push(Fragment {
                    text: "\n".to_owned(),
                    role: Role::Whitespace,
                    provenance: Provenance::Generated,
                });
            } else if let Some(replacement) = spacing {
                if replacement != text && !replacement.is_empty() {
                    fragments.push(Fragment {
                        text: replacement.to_owned(),
                        role: Role::Whitespace,
                        provenance: Provenance::Generated,
                    });
                } else if replacement == text {
                    append_source(&mut fragments, source, gap, Role::Whitespace, input);
                }
            } else {
                append_source(&mut fragments, source, gap, Role::Literal, input);
            }
            if source.get(token.range.clone()).is_none() {
                return false;
            }
            append_source(
                &mut fragments,
                source,
                token.range.clone(),
                token.role,
                input,
            );
            if document.fragments.len().saturating_add(fragments.len()) > budget.max_fragments {
                return false;
            }
            offset = token.range.end;
            previous = Some(token);
        }
        append_source(
            &mut fragments,
            source,
            offset..source.len(),
            Role::Literal,
            input,
        );
        if document.fragments.len().saturating_add(fragments.len()) > budget.max_fragments {
            return false;
        }
        document.fragments.extend(fragments);
        true
    }
}

fn parse(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    // The callback bounds parser work even for adversarial incomplete input. Cancellation
    // drops this invocation's parser; no suspended parser state survives into another call.
    let started = Instant::now();
    let mut progress = |_state: &tree_sitter::ParseState| {
        if started.elapsed() >= Duration::from_millis(20) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut read =
        |offset: usize, _: tree_sitter::Point| source.as_bytes().get(offset..).unwrap_or_default();
    parser.parse_with_options(
        &mut read,
        None,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )
}

fn collect_statements(
    root: Node<'_>,
    source_bytes: usize,
    max_tokens: usize,
) -> Option<Vec<Token<'_>>> {
    let mut tokens = Vec::new();
    let mut visits = 0;
    // A recovered module may contain complete statements before an unfinished suffix.
    // Never descend into an erroneous statement: missing zero-width tokens are not source.
    for index in 0..root.child_count() {
        let child = root.child(index)?;
        if !collect(
            child,
            max_tokens,
            source_bytes.saturating_add(1).saturating_mul(8),
            &mut visits,
            &mut tokens,
        ) {
            return None;
        }
    }
    Some(tokens)
}

fn collect<'tree>(
    node: Node<'tree>,
    max_tokens: usize,
    max_byte_visits: usize,
    visits: &mut usize,
    tokens: &mut Vec<Token<'tree>>,
) -> bool {
    // Extraction nesting is unrelated to grammar depth. A cursor avoids Rust stack growth;
    // byte and fragment budgets bound total traversal without a small structural ceiling.
    let max_visits = max_byte_visits.min(max_tokens.saturating_mul(8));
    let mut cursor = node.walk();
    loop {
        *visits = visits.saturating_add(1);
        if *visits > max_visits || tokens.len() >= max_tokens {
            return false;
        }
        let node = cursor.node();
        let kind = node.kind();
        let certain = !node.has_error() && !node.is_missing() && !node.is_error();
        let opaque = !certain
            || matches!(
                kind,
                "string" | "concatenated_string" | "comment" | "line_continuation"
            );
        if !opaque && cursor.goto_first_child() {
            continue;
        }
        if node.end_byte() > node.start_byte() {
            tokens.push(Token {
                node,
                range: node.byte_range(),
                role: if !certain {
                    Role::Literal
                } else if matches!(kind, "string" | "concatenated_string") {
                    Role::String
                } else if kind == "comment" {
                    Role::Comment
                } else if node.is_named() {
                    Role::Literal
                } else {
                    Role::Operator
                },
                certain: certain && kind != "line_continuation",
            });
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return true;
            }
        }
    }
}

fn append_source(
    fragments: &mut Vec<Fragment>,
    source: &str,
    range: Range<usize>,
    role: Role,
    input: &AdapterInput,
) {
    if range.is_empty() {
        return;
    }
    fragments.push(Fragment {
        text: source[range.clone()].to_owned(),
        role,
        provenance: Provenance::Source {
            id: input.source,
            range: input.range.start + range.start..input.range.start + range.end,
        },
    });
}

// Do not turn a source line break into a gap: Python indentation is semantic. Unsupported
// whitespace, comments and recovery boundaries are literal even when neighboring tokens parse.
fn spacing(left: &Token<'_>, right: &Token<'_>, gap: &str, source: &str) -> Option<&'static str> {
    if !left.certain
        || !right.certain
        || left.role == Role::Comment
        || right.role == Role::Comment
        || !gap.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
    {
        return None;
    }
    let a = source.get(left.range.clone())?;
    let b = source.get(right.range.clone())?;
    if matches!(b, "," | ";" | ")" | "]" | "}") || matches!(a, "(" | "[" | "{") {
        return Some("");
    }
    if a == "," || a == ";" || padded_operator(left) || padded_operator(right) {
        return Some(" ");
    }
    // Call/subscript/member adjacency and colon rules depend on syntax context; keep them
    // literal rather than making guesses about slices, annotations or compound statements.
    None
}

fn padded_operator(token: &Token<'_>) -> bool {
    if token.node.is_named() {
        return false;
    }
    token.node.parent().is_some_and(|parent| {
        matches!(
            parent.kind(),
            "binary_operator"
                | "boolean_operator"
                | "comparison_operator"
                | "assignment"
                | "augmented_assignment"
                | "named_expression"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::document::SourceId;
    use super::*;

    fn present(source: &str) -> Document {
        let mut document = Document::original(source);
        assert!(PythonAdapter.present(
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

    #[test]
    fn beautifies_gaps_without_changing_execution_or_indentation() {
        let source = "if ready:\n\tresult=call( 1,2 ) # exact comment\n";
        let document = present(source);
        assert_eq!(document.execution_text(), source);
        assert_eq!(
            document.display_text(),
            "if ready:\n\tresult = call(1, 2) # exact comment\n"
        );
        assert!(
            document
                .fragments
                .iter()
                .any(|fragment| fragment.provenance == Provenance::Generated)
        );
        for fragment in &document.fragments {
            if let Provenance::Source { id, range } = &fragment.provenance {
                assert_eq!(fragment.text, document.sources[id.0].text[range.clone()]);
            } else {
                assert!(fragment.text.chars().all(char::is_whitespace));
            }
        }
    }

    #[test]
    fn preserves_strings_unicode_comments_and_physical_newlines() {
        let source = "x='界🙂 e\u{301}'; y = f\"{value= }\"\r\nz='''a\n  b''' # untouched\n";
        let document = present(source);
        assert_eq!(
            document.display_text(),
            "x = '界🙂 e\u{301}';\ny = f\"{value= }\"\r\nz = '''a\n  b''' # untouched\n"
        );
    }

    #[test]
    fn splits_only_proven_module_statement_boundaries() {
        assert_eq!(
            present("x=1;y=2; print(x,y)").display_text(),
            "x = 1;\ny = 2;\nprint(x, y)"
        );
        assert_eq!(
            present("if ready: x=1; y=2\n").display_text(),
            "if ready: x = 1; y = 2\n"
        );
        assert_eq!(
            present("if ready:\n  x=1; y=2\n").display_text(),
            "if ready:\n  x = 1; y = 2\n"
        );
    }

    #[test]
    fn incomplete_suffix_stays_literal_while_complete_prefix_progresses() {
        let document = present("x=1\ny = (\n");
        assert!(document.display_text().starts_with("x = 1\n"));
        assert!(document.display_text().ends_with("y = (\n"));
    }

    #[test]
    fn extraction_depth_does_not_limit_compound_syntax_depth() {
        let source = "if ready:\n    result=call(outer(inner(1+2)),3)\n";
        let mut document = Document::original(source);
        let budget = Budget {
            max_depth: 4,
            ..Budget::default()
        };
        assert!(PythonAdapter.present(
            &mut document,
            &AdapterInput {
                source: SourceId(0),
                range: 0..source.len(),
                depth: 1,
            },
            &budget
        ));
        assert_eq!(
            document.display_text(),
            "if ready:\n    result = call(outer(inner(1 + 2)), 3)\n"
        );
        assert_eq!(document.execution_text(), source);

        let before = document.clone();
        assert!(!PythonAdapter.present(
            &mut document,
            &AdapterInput {
                source: SourceId(0),
                range: 0..source.len(),
                depth: 4,
            },
            &budget
        ));
        assert_eq!(document, before);
    }

    #[test]
    fn budgets_fail_atomically() {
        let mut document = Document::original("x=1");
        let before = document.clone();
        let input = AdapterInput {
            source: SourceId(0),
            range: 0..3,
            depth: 0,
        };
        for budget in [
            Budget {
                max_source_bytes: 2,
                ..Budget::default()
            },
            Budget {
                max_fragments: 1,
                ..Budget::default()
            },
            Budget {
                max_depth: 0,
                ..Budget::default()
            },
        ] {
            assert!(!PythonAdapter.present(&mut document, &input, &budget));
            assert_eq!(document, before);
        }
    }

    #[test]
    fn decoded_subrange_keeps_decoded_byte_authority() {
        let mut document = Document::original("python -c 'x=1'");
        let id = document
            .add_decoded("prefix\nx=1\n".to_owned(), SourceId(0), 10..15)
            .unwrap();
        assert!(PythonAdapter.present(
            &mut document,
            &AdapterInput {
                source: id,
                range: 7..11,
                depth: 1,
            },
            &Budget::default()
        ));
        assert_eq!(document.execution_text(), "python -c 'x=1'");
        assert_eq!(document.display_text(), "x = 1\n");
        assert!(
            document
                .fragments
                .iter()
                .all(|fragment| match &fragment.provenance {
                    Provenance::Generated => true,
                    Provenance::Source { id: actual, range } => *actual == id && range.start >= 7,
                })
        );
    }
}
