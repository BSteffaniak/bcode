//! Source-backed command presentation; executed and copied bytes remain original.

pub mod document;
mod extraction;
#[cfg(feature = "embedded-javascript")]
mod javascript;
#[cfg(feature = "embedded-python")]
mod python;
mod shell;

use bcode_config::ShellOutputPresentationPolicy;
use document::{
    AdapterInput, Budget, Correspondence, Document, InputPolicy, LanguageAdapter, Provenance, Role,
    SourceId,
};

/// Present the same progressive source policy in every command lifecycle stage.
#[must_use]
pub fn present(source: &str, policy: &ShellOutputPresentationPolicy) -> Document {
    let budget = Budget {
        max_source_bytes: policy.max_source_bytes,
        max_fragments: policy.max_source_bytes.saturating_mul(4),
        max_depth: policy.max_depth,
    };
    let input_policy = if policy.enabled {
        InputPolicy::Format
    } else {
        InputPolicy::Literal
    };
    let mut document = shell::present(source, input_policy, &budget);
    if policy.enabled && policy.embedded && source.len() <= budget.max_source_bytes {
        let mut remaining = policy.max_regions;
        embed(
            &mut document,
            SourceId(0),
            policy,
            &budget,
            0,
            &mut remaining,
        );
    }
    document
}

fn embed(
    document: &mut Document,
    parent: SourceId,
    policy: &ShellOutputPresentationPolicy,
    budget: &Budget,
    depth: usize,
    remaining: &mut usize,
) {
    if depth >= budget.max_depth || *remaining == 0 {
        return;
    }
    let source = document.sources[parent.0].text.clone();
    let old_sources = document.sources.len();
    let original = document.fragments.clone();
    let invocations = shell::invocations(&source, budget);
    for invocation in invocations {
        if *remaining == 0 {
            break;
        }
        let Some(extracted) = extraction::extract(&source, &invocation, budget.max_source_bytes)
        else {
            continue;
        };
        if extracted.uncertain {
            continue;
        }
        let adapter: &dyn LanguageAdapter = match extracted.language {
            #[cfg(feature = "embedded-python")]
            extraction::Language::Python if policy.python => &python::PythonAdapter,
            #[cfg(feature = "embedded-javascript")]
            extraction::Language::JavaScript if policy.javascript => &javascript::JavaScriptAdapter,
            extraction::Language::Shell if policy.nested_shell => &shell::ShellAdapter,
            _ => continue,
        };
        let old_sources = document.sources.len();
        let correspondence = extracted
            .correspondence
            .into_iter()
            .map(|mapping| Correspondence {
                decoded: mapping.decoded,
                original: mapping.original,
            })
            .collect();
        let Some(id) = document.add_decoded_mapped(
            extracted.text,
            parent,
            extracted.original.clone(),
            correspondence,
        ) else {
            continue;
        };
        let start = document.fragments.len();
        let input = AdapterInput {
            source: id,
            range: 0..document.sources[id.0].text.len(),
            depth: depth + 1,
        };
        if !adapter.present(document, &input, budget) {
            document.fragments.truncate(start);
            document.sources.truncate(old_sources);
            continue;
        }
        let replacement = document.fragments.split_off(start);
        if !replace(document, parent, extracted.original, replacement) {
            document.sources.truncate(old_sources);
            continue;
        }
        *remaining -= 1;
        if extracted.language == extraction::Language::Shell {
            embed(document, id, policy, budget, depth + 1, remaining);
        }
    }
    let display_bytes: usize = document
        .fragments
        .iter()
        .map(|fragment| fragment.text.len())
        .sum();
    if document.fragments.len() > budget.max_fragments
        || display_bytes > budget.max_source_bytes.saturating_mul(8)
    {
        document.fragments = original;
        document.sources.truncate(old_sources);
    }
}

fn replace(
    document: &mut Document,
    parent: SourceId,
    encoded: std::ops::Range<usize>,
    replacement: Vec<document::Fragment>,
) -> bool {
    // The encoded region must remain a contiguous literal source island. Do not replace across
    // already generated geometry or recovered/decoded fragments, nor hide quote delimiters.
    let Some(index) = document.fragments.iter().position(|fragment| matches!(&fragment.provenance,
        Provenance::Source { id, range } if *id == parent && range.start <= encoded.start && range.end >= encoded.end)) else { return false; };
    let old = document.fragments[index].clone();
    let Provenance::Source { range, .. } = old.provenance else {
        return false;
    };
    let source = &document.sources[parent.0].text;
    let mut fragments = Vec::new();
    if range.start < encoded.start {
        fragments.push(document::Fragment {
            text: source[range.start..encoded.start].to_owned(),
            role: old.role,
            provenance: Provenance::Source {
                id: parent,
                range: range.start..encoded.start,
            },
        });
    }
    // Keep shell spelling visible at the boundary; decoded presentation is explicitly separate.
    fragments.push(document::Fragment {
        text: "\n".to_owned(),
        role: Role::Whitespace,
        provenance: Provenance::Generated,
    });
    for mut fragment in replacement {
        // An uncertain embedded island retains the parent shell spelling's semantic role;
        // successful local grammar classification may refine it, never erase it wholesale.
        if fragment.role == Role::Literal {
            fragment.role = old.role;
        }
        fragments.push(fragment);
    }
    if encoded.end < range.end {
        fragments.push(document::Fragment {
            text: source[encoded.end..range.end].to_owned(),
            role: old.role,
            provenance: Provenance::Source {
                id: parent,
                range: encoded.end..range.end,
            },
        });
    }
    document.fragments.splice(index..=index, fragments);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_recovery_retains_parent_roles_and_exact_copy_mapping_for_prefixes() {
        for source in [
            "python3 -c 'value=42;print(value); result=(界'",
            "node -e 'const value=42;print(value); let result=(界'",
        ] {
            for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                let prefix = &source[..end];
                let document = present(prefix, &ShellOutputPresentationPolicy::default());
                assert_eq!(document.execution_text(), prefix);
                assert_eq!(
                    document,
                    present(prefix, &ShellOutputPresentationPolicy::default())
                );
                for fragment in &document.fragments {
                    match &fragment.provenance {
                        Provenance::Source { id, range } => {
                            assert_eq!(fragment.text, document.sources[id.0].text[range.clone()]);
                            let ranges = document.original_ranges(*id, range.clone()).unwrap();
                            assert!(
                                ranges
                                    .iter()
                                    .all(|range| prefix.get(range.clone()).is_some())
                            );
                        }
                        Provenance::Generated => assert!(fragment.text.trim().is_empty()),
                    }
                }
                if end >= source.find("42").unwrap() + 2 {
                    assert!(
                        document
                            .fragments
                            .iter()
                            .any(|fragment| fragment.text == "42" && fragment.role == Role::Number),
                        "{prefix}: {}",
                        document.display_text()
                    );
                }
            }
        }
    }

    #[test]
    fn uncertain_embedded_projection_inherits_parent_role() {
        let source = "node -e 'unknown'";
        let mut document = Document::original(source);
        document.push_source(SourceId(0), 0..source.len(), Role::String);
        let encoded = 9..16;
        let replacement = vec![document::Fragment {
            text: source[encoded.clone()].to_owned(),
            role: Role::Literal,
            provenance: Provenance::Source {
                id: SourceId(0),
                range: encoded.clone(),
            },
        }];
        assert!(replace(&mut document, SourceId(0), encoded, replacement));
        assert!(
            document
                .fragments
                .iter()
                .filter(|fragment| matches!(fragment.provenance, Provenance::Source { .. }))
                .all(|fragment| fragment.role == Role::String)
        );
        assert_eq!(document.display_text(), "node -e '\nunknown'");
    }

    #[test]
    fn embeds_interpreter_source_and_keeps_copy_authority() {
        for source in [
            "python3 -c 'x=1;print(x)'",
            "node -e 'if(x){foo();bar()}'",
            "bash -c 'if true; then echo yes; fi'",
        ] {
            let document = present(source, &ShellOutputPresentationPolicy::default());
            assert_eq!(document.execution_text(), source);
            assert!(
                document.sources.len() > 1,
                "{source}: {}",
                document.display_text()
            );
            assert!(document.display_text().contains('\n'));
        }
    }

    #[test]
    fn disabled_and_dynamic_embedded_inputs_do_not_claim_decoded_sources() {
        let source = "python3 -c \"print($value)\"";
        assert_eq!(
            present(source, &ShellOutputPresentationPolicy::default())
                .sources
                .len(),
            1
        );
        let policy = ShellOutputPresentationPolicy {
            embedded: false,
            ..Default::default()
        };
        assert_eq!(present("node -e 'foo();bar()'", &policy).sources.len(), 1);
    }
}
