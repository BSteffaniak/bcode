//! Syntax-aware Python display formatting, never execution or clipboard authority.
//!
//! Ruff rewrites live in a separate formatted source without exact original-byte mapping.
//! Invalid/incomplete programs retain their literal bytes with tolerant syntax highlighting.

use std::ops::{ControlFlow, Range};

use ruff_formatter::LineWidth;
use ruff_python_formatter::{PyFormatOptions, format_module_source};
use tree_sitter::{Node, ParseOptions, Parser};

use super::document::{
    AdapterFailure, AdapterInput, Budget, Document, Fragment, LanguageAdapter, Provenance, Role,
    SourceId,
};

// These admission limits bound input and tree work, not formatter elapsed time. Ruff has no
// cancellation callback. Keep syntax depth independent of shell/language composition depth.
const MAX_FORMAT_SOURCE_BYTES: usize = 16 * 1024;
const MAX_FORMAT_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_SYNTAX_DEPTH: usize = 64;
const MAX_TREE_VISITS: usize = 32 * 1024;

/// Optional compiled Python presentation adapter, independent of shell execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct PythonAdapter;

impl PythonAdapter {
    /// Highlight literal Python bytes without formatting or invoking Ruff.
    ///
    /// Returns false without changing the document when admission or projection fails.
    #[must_use]
    pub fn present_highlight(
        document: &mut Document,
        input: &AdapterInput,
        budget: &Budget,
    ) -> bool {
        highlight(document, input, budget).is_ok()
    }
}

impl LanguageAdapter for PythonAdapter {
    fn present_outcome(
        &self,
        document: &mut Document,
        input: &AdapterInput,
        budget: &Budget,
    ) -> Result<(), AdapterFailure> {
        let source = admitted_source(document, input, budget)?;
        let tree = parse(source).ok_or(AdapterFailure::BudgetExceeded)?;
        preflight(tree.root_node(), source.len(), budget.max_fragments)?;
        if tree.root_node().has_error() {
            return highlight_tree(document, input, budget, &tree);
        }
        // Options are deliberately config-free and independent of terminal geometry.
        let options = PyFormatOptions::default()
            .with_line_width(LineWidth::try_from(88).map_err(|_| AdapterFailure::Unavailable)?);
        let Ok(printed) = format_module_source(source, options) else {
            // Ruff and tree-sitter recognize different Python versions/recovery boundaries.
            // A Ruff rejection is not permission to repair the source or lose its highlighting.
            return highlight_tree(document, input, budget, &tree);
        };
        let output = printed.into_code();
        if output.len() > MAX_FORMAT_OUTPUT_BYTES {
            return Err(AdapterFailure::BudgetExceeded);
        }
        let formatted_tree = parse(&output).ok_or(AdapterFailure::BudgetExceeded)?;
        preflight(
            formatted_tree.root_node(),
            output.len(),
            budget.max_fragments,
        )?;
        if formatted_tree.root_node().has_error() {
            return highlight_tree(document, input, budget, &tree);
        }
        // Build everything locally first. The provisional identity is the next retained source;
        // add_formatted validates the parent before mutation. No fallible work follows it.
        let formatted_input = AdapterInput {
            source: SourceId(document.sources.len()),
            range: 0..output.len(),
            depth: input.depth,
        };
        let fragments = project(
            &output,
            formatted_tree.root_node(),
            &formatted_input,
            budget
                .max_fragments
                .saturating_sub(document.fragments.len()),
        )?;
        document
            .add_formatted(output, input.source, input.range.clone())
            .ok_or(AdapterFailure::Unavailable)?;
        document.fragments.extend(fragments);
        Ok(())
    }

    fn present(&self, document: &mut Document, input: &AdapterInput, budget: &Budget) -> bool {
        self.present_outcome(document, input, budget).is_ok()
    }
}

fn admitted_source<'a>(
    document: &'a Document,
    input: &AdapterInput,
    budget: &Budget,
) -> Result<&'a str, AdapterFailure> {
    let source = document
        .sources
        .get(input.source.0)
        .and_then(|source| source.text.get(input.range.clone()))
        .ok_or(AdapterFailure::Unavailable)?;
    if source.len() > budget.max_source_bytes.min(MAX_FORMAT_SOURCE_BYTES)
        || input.depth >= budget.max_depth
    {
        return Err(AdapterFailure::BudgetExceeded);
    }
    Ok(source)
}

fn highlight(
    document: &mut Document,
    input: &AdapterInput,
    budget: &Budget,
) -> Result<(), AdapterFailure> {
    let source = admitted_source(document, input, budget)?;
    let tree = parse(source).ok_or(AdapterFailure::BudgetExceeded)?;
    preflight(tree.root_node(), source.len(), budget.max_fragments)?;
    highlight_tree(document, input, budget, &tree)
}

fn highlight_tree(
    document: &mut Document,
    input: &AdapterInput,
    budget: &Budget,
    tree: &tree_sitter::Tree,
) -> Result<(), AdapterFailure> {
    let source = admitted_source(document, input, budget)?;
    let fragments = project(
        source,
        tree.root_node(),
        input,
        budget
            .max_fragments
            .saturating_sub(document.fragments.len()),
    )?;
    document.fragments.extend(fragments);
    Ok(())
}

fn parse(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .ok()?;
    // Cancellation drops this invocation's parser rather than retaining suspended parse state.
    let mut checks = 0_usize;
    let mut progress = |_state: &tree_sitter::ParseState| {
        checks += 1;
        if checks > 256 {
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

/// Inspect all nodes, including string/interpolation interiors, before Ruff's recursive work.
/// Cursor traversal itself does not recurse on the Rust stack.
fn preflight(root: Node<'_>, bytes: usize, fragments: usize) -> Result<(), AdapterFailure> {
    let max_visits = MAX_TREE_VISITS
        .min(bytes.saturating_add(1).saturating_mul(8))
        .min(fragments.saturating_mul(8));
    let mut cursor = root.walk();
    let mut depth = 0_usize;
    let mut visits = 0_usize;
    loop {
        visits += 1;
        if visits > max_visits || depth > MAX_SYNTAX_DEPTH {
            return Err(AdapterFailure::BudgetExceeded);
        }
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

fn project(
    source: &str,
    root: Node<'_>,
    input: &AdapterInput,
    max_fragments: usize,
) -> Result<Vec<Fragment>, AdapterFailure> {
    let tokens = collect_statements(root, source.len(), max_fragments)
        .ok_or(AdapterFailure::BudgetExceeded)?;
    let mut fragments = Vec::new();
    let mut offset = 0;
    for (range, role) in tokens {
        if range.start < offset || source.get(range.clone()).is_none() {
            return Err(AdapterFailure::Unavailable);
        }
        append_source(
            &mut fragments,
            source,
            offset..range.start,
            Role::Literal,
            input,
        );
        append_source(&mut fragments, source, range.clone(), role, input);
        if fragments.len() > max_fragments {
            return Err(AdapterFailure::BudgetExceeded);
        }
        offset = range.end;
    }
    append_source(
        &mut fragments,
        source,
        offset..source.len(),
        Role::Literal,
        input,
    );
    if fragments.len() > max_fragments {
        return Err(AdapterFailure::BudgetExceeded);
    }
    Ok(fragments)
}

fn collect_statements(
    root: Node<'_>,
    source_bytes: usize,
    max_tokens: usize,
) -> Option<Vec<(Range<usize>, Role)>> {
    let mut tokens = Vec::new();
    let mut visits = 0;
    // Recovery containers remain traversable; missing leaves contribute no invented bytes.
    // Walk from the root rather than indexing each child (wide modules stay linear).
    if !collect(
        root,
        max_tokens,
        source_bytes.saturating_add(1).saturating_mul(8),
        &mut visits,
        &mut tokens,
    ) {
        return None;
    }
    Some(tokens)
}

fn collect(
    node: Node<'_>,
    max_tokens: usize,
    max_byte_visits: usize,
    visits: &mut usize,
    tokens: &mut Vec<(Range<usize>, Role)>,
) -> bool {
    let max_visits = max_byte_visits.min(max_tokens.saturating_mul(8));
    let mut cursor = node.walk();
    loop {
        *visits = visits.saturating_add(1);
        if *visits > max_visits || tokens.len() >= max_tokens {
            return false;
        }
        let node = cursor.node();
        let kind = node.kind();
        let opaque = matches!(
            kind,
            "string" | "concatenated_string" | "comment" | "line_continuation"
        );
        if !opaque && cursor.goto_first_child() {
            continue;
        }
        if node.end_byte() > node.start_byte() {
            let role = if node.is_missing() || (node.is_error() && node.child_count() == 0) {
                Role::Literal
            } else if matches!(kind, "string" | "concatenated_string") {
                Role::String
            } else if kind == "comment" {
                Role::Comment
            } else if kind == "identifier" {
                embedded_identifier_role(node)
            } else if matches!(kind, "integer" | "float") {
                Role::Number
            } else if node.is_named() {
                Role::Literal
            } else if kind
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            {
                Role::Keyword
            } else {
                Role::Operator
            };
            tokens.push((node.byte_range(), role));
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return true;
            }
        }
    }
}

fn embedded_identifier_role(node: Node<'_>) -> Role {
    if let Some(parent) = node.parent()
        && ((parent.kind() == "call" && parent.child_by_field_name("function") == Some(node))
            || (parent.kind() == "function_definition"
                && parent.child_by_field_name("name") == Some(node)))
    {
        Role::Function
    } else {
        Role::Identifier
    }
}

fn append_source(
    fragments: &mut Vec<Fragment>,
    source: &str,
    range: Range<usize>,
    role: Role,
    input: &AdapterInput,
) {
    if !range.is_empty() {
        fragments.push(Fragment {
            text: source[range.clone()].to_owned(),
            role,
            provenance: Provenance::Source {
                id: input.source,
                range: input.range.start + range.start..input.range.start + range.end,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::document::SourceOrigin;
    use super::*;

    fn input(source: &str) -> AdapterInput {
        AdapterInput {
            source: SourceId(0),
            range: 0..source.len(),
            depth: 0,
        }
    }

    fn present(source: &str) -> Document {
        let mut document = Document::original(source);
        assert!(PythonAdapter.present(&mut document, &input(source), &Budget::default()));
        assert_eq!(document.execution_text(), source);
        for fragment in &document.fragments {
            let Provenance::Source { id, range } = &fragment.provenance else {
                panic!("Python display bytes must reference a retained source");
            };
            assert_eq!(fragment.text, document.sources[id.0].text[range.clone()]);
        }
        document
    }

    #[test]
    fn formats_suites_and_module_statements() {
        assert_eq!(
            present("if ready: x=1; print( x )\n").display_text(),
            "if ready:\n    x = 1\n    print(x)\n"
        );
        assert_eq!(
            present("if ready:\n  x=1; y=2\n").display_text(),
            "if ready:\n    x = 1\n    y = 2\n"
        );
        assert_eq!(
            present("x=1;y=2; print(x,y)").display_text(),
            "x = 1\ny = 2\nprint(x, y)\n"
        );
    }

    #[test]
    fn formats_dictionary_slice_defaults_annotations_and_comprehension() {
        let source = "def f(x:int=1)->list[int]:\n return [v*2 for v in {'a':x,'b':2}.values() if v>0][1:3:2]\n";
        assert_eq!(
            present(source).display_text(),
            "def f(x: int = 1) -> list[int]:\n    return [v * 2 for v in {\"a\": x, \"b\": 2}.values() if v > 0][1:3:2]\n"
        );
    }

    #[test]
    fn expands_long_lists_at_fixed_width() {
        let source = "values=[1111111111,2222222222,3333333333,4444444444,5555555555,6666666666,7777777777,8888888888]\n";
        assert_eq!(
            present(source).display_text(),
            "values = [\n    1111111111,\n    2222222222,\n    3333333333,\n    4444444444,\n    5555555555,\n    6666666666,\n    7777777777,\n    8888888888,\n]\n"
        );
    }

    #[test]
    fn rewritten_strings_have_display_provenance_not_original_mapping() {
        let source = "s='界🙂 e\u{301}'; x=(1) # comment\n";
        let document = present(source);
        assert_eq!(
            document.display_text(),
            "s = \"界🙂 e\u{301}\"\nx = 1  # comment\n"
        );
        assert!(matches!(
            document.sources[1].origin,
            SourceOrigin::Formatted {
                parent: SourceId(0),
                ..
            }
        ));
        assert_eq!(
            document.original_ranges(SourceId(1), 0..document.sources[1].text.len()),
            None
        );
        assert!(
            document
                .fragments
                .iter()
                .any(|fragment| fragment.role == Role::String)
        );
        assert!(
            document
                .fragments
                .iter()
                .any(|fragment| fragment.role == Role::Comment)
        );
    }

    #[test]
    fn invalid_and_incomplete_programs_remain_literal_and_highlighted() {
        for source in [
            "if ready:\n  values=[1,",
            "def broken(:\n",
            "x=42; print(x); result=(界",
        ] {
            let document = present(source);
            assert_eq!(document.display_text(), source);
            assert_eq!(document.sources.len(), 1);
        }
        let source = "value=42; print(value); result=(界";
        for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
            let prefix = &source[..end];
            let mut document = Document::original(prefix);
            assert!(PythonAdapter::present_highlight(
                &mut document,
                &input(prefix),
                &Budget::default()
            ));
            assert_eq!(document.display_text(), prefix);
            assert_eq!(document.sources.len(), 1);
        }
        assert!(
            present(source)
                .fragments
                .iter()
                .any(|fragment| fragment.text == "42" && fragment.role == Role::Number)
        );
    }

    #[test]
    fn disabled_formatting_is_literal_even_for_valid_suites() {
        let source = "if ready: x=1; print( x )\n";
        let mut document = Document::original(source);
        assert!(PythonAdapter::present_highlight(
            &mut document,
            &input(source),
            &Budget::default()
        ));
        assert_eq!(document.display_text(), source);
        assert_eq!(document.sources.len(), 1);
    }

    #[test]
    fn budget_failures_are_transactional() {
        let source = "x=1";
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
            let mut document = Document::original(source);
            let before = document.clone();
            assert_eq!(
                PythonAdapter.present_outcome(&mut document, &input(source), &budget),
                Err(AdapterFailure::BudgetExceeded)
            );
            assert_eq!(document, before);
        }
        for source in [
            "x".repeat(MAX_FORMAT_SOURCE_BYTES + 1),
            format!("x={}1{}", "(".repeat(128), ")".repeat(128)),
            format!("x={}1", "not ".repeat(128)),
        ] {
            let mut document = Document::original(&source);
            let before = document.clone();
            assert_eq!(
                PythonAdapter.present_outcome(&mut document, &input(&source), &Budget::default()),
                Err(AdapterFailure::BudgetExceeded)
            );
            assert_eq!(document, before);
        }
    }

    #[test]
    fn formatted_subrange_retains_parent_association_and_exact_command_copy() {
        let mut document = Document::original("python -c 'x=1'");
        let decoded = document
            .add_decoded("prefix\nx=1\n".to_owned(), SourceId(0), 10..15)
            .unwrap();
        assert!(PythonAdapter.present(
            &mut document,
            &AdapterInput {
                source: decoded,
                range: 7..11,
                depth: 1
            },
            &Budget::default()
        ));
        assert_eq!(document.execution_text(), "python -c 'x=1'");
        assert_eq!(document.display_text(), "x = 1\n");
        assert_eq!(
            document.sources[2].origin,
            SourceOrigin::Formatted {
                parent: decoded,
                range: 7..11
            }
        );
        assert_eq!(document.original_ranges(SourceId(2), 0..6), None);
        assert_eq!(document.original_region(SourceId(2)), Some(10..15));
    }
}
