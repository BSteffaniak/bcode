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
    AdapterFailure, AdapterInput, Budget, Correspondence, Document, InputPolicy, LanguageAdapter,
    PresentationIssue, Provenance, Role, SourceId,
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
    let projection = shell::analyze(source, input_policy, &budget);
    let mut document = projection.document;
    if policy.embedded && source.len() <= budget.max_source_bytes {
        let mut remaining = policy.max_regions;
        embed(
            &mut document,
            SourceId(0),
            projection.invocations,
            policy,
            &budget,
            0,
            &mut remaining,
        );
    }
    document
}

fn present_nested(
    document: &mut Document,
    input: &AdapterInput,
    policy: &ShellOutputPresentationPolicy,
    budget: &Budget,
    remaining: &mut usize,
) {
    let projection = shell::analyze(
        &document.sources[input.source.0].text,
        if policy.enabled {
            InputPolicy::Format
        } else {
            InputPolicy::Literal
        },
        budget,
    );
    for mut issue in projection.document.issues {
        issue.source = input.source;
        document.issues.push(issue);
    }
    for mut fragment in projection.document.fragments {
        if let Provenance::Source { id, .. } = &mut fragment.provenance {
            *id = input.source;
        }
        document.fragments.push(fragment);
    }
    embed(
        document,
        input.source,
        projection.invocations,
        policy,
        budget,
        input.depth,
        remaining,
    );
}

fn rollback(
    document: &mut Document,
    checkpoint: (Vec<document::Fragment>, usize, usize),
    issue: PresentationIssue,
) {
    document.fragments = checkpoint.0;
    document.sources.truncate(checkpoint.1);
    document.issues.truncate(checkpoint.2);
    document.issues.push(issue);
}

fn embed(
    document: &mut Document,
    parent: SourceId,
    invocations: Vec<extraction::Invocation>,
    policy: &ShellOutputPresentationPolicy,
    budget: &Budget,
    depth: usize,
    remaining: &mut usize,
) {
    if depth >= budget.max_depth || *remaining == 0 {
        return;
    }
    let source = document.sources[parent.0].text.clone();
    for invocation in invocations {
        if *remaining == 0 {
            break;
        }
        let Some(extracted) = extraction::extract(&source, &invocation, budget.max_source_bytes)
            .filter(|extracted| !extracted.uncertain)
        else {
            continue;
        };
        let adapter: Option<&dyn LanguageAdapter> = match extracted.language {
            #[cfg(feature = "embedded-python")]
            extraction::Language::Python if policy.python => Some(&python::PythonAdapter),
            #[cfg(feature = "embedded-javascript")]
            extraction::Language::JavaScript if policy.javascript => {
                Some(&javascript::JavaScriptAdapter)
            }
            extraction::Language::Shell if policy.nested_shell => None,
            _ => continue,
        };
        let old_sources = document.sources.len();
        let old_issues = document.issues.len();
        let original = document.fragments.clone();
        let encoded = extracted.original.clone();
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
            document.issues.push(PresentationIssue {
                source: parent,
                range: encoded,
                reason: AdapterFailure::Unavailable,
            });
            continue;
        };
        let start = document.fragments.len();
        let input = AdapterInput {
            source: id,
            range: 0..document.sources[id.0].text.len(),
            depth: depth + 1,
        };
        let nested_allowance = remaining.saturating_sub(1);
        let mut nested_remaining = nested_allowance;
        let accepted = present_child(
            document,
            &input,
            budget,
            policy,
            extracted.language,
            adapter,
            &mut nested_remaining,
        );
        if let Err(reason) = accepted {
            rollback(
                document,
                (original, old_sources, old_issues),
                PresentationIssue {
                    source: parent,
                    range: encoded,
                    reason,
                },
            );
            continue;
        }
        if let Err(reason) = compose(
            document,
            parent,
            id,
            extracted.original,
            start,
            policy,
            budget,
        ) {
            rollback(
                document,
                (original, old_sources, old_issues),
                PresentationIssue {
                    source: parent,
                    range: encoded,
                    reason,
                },
            );
            continue;
        }
        *remaining = nested_remaining;
    }
}

fn present_child(
    document: &mut Document,
    input: &AdapterInput,
    budget: &Budget,
    policy: &ShellOutputPresentationPolicy,
    language: extraction::Language,
    adapter: Option<&dyn LanguageAdapter>,
    remaining: &mut usize,
) -> Result<(), AdapterFailure> {
    if input.depth >= budget.max_depth {
        return Err(AdapterFailure::BudgetExceeded);
    }
    if !policy.enabled && language == extraction::Language::Python {
        #[cfg(feature = "embedded-python")]
        {
            return if python::PythonAdapter::present_highlight(document, input, budget) {
                Ok(())
            } else {
                Err(AdapterFailure::BudgetExceeded)
            };
        }
        #[cfg(not(feature = "embedded-python"))]
        {
            return Err(AdapterFailure::Unavailable);
        }
    }
    if let Some(adapter) = adapter {
        adapter.present_outcome(document, input, budget)
    } else {
        present_nested(document, input, policy, budget, remaining);
        Ok(())
    }
}

// Commit the admitted child projection and enforce final display budgets. The caller retains
// the checkpoint so either failure restores the complete local presentation transaction.
fn compose(
    document: &mut Document,
    parent: SourceId,
    decoded: SourceId,
    encoded: std::ops::Range<usize>,
    start: usize,
    policy: &ShellOutputPresentationPolicy,
    budget: &Budget,
) -> Result<(), AdapterFailure> {
    let replacement = document.fragments.split_off(start);
    let replaced = if policy.enabled {
        replace(document, parent, encoded, replacement)
    } else {
        refine(document, parent, decoded, encoded, &replacement)
    };
    let display_bytes: usize = document
        .fragments
        .iter()
        .map(|fragment| fragment.text.len())
        .sum();
    if document.fragments.len() > budget.max_fragments
        || display_bytes > budget.max_source_bytes.saturating_mul(8)
    {
        Err(AdapterFailure::BudgetExceeded)
    } else if replaced {
        Ok(())
    } else {
        Err(AdapterFailure::Unavailable)
    }
}

// Admit only one display-contiguous, source-contiguous island. Validate before mutation,
// including duplicate coverage elsewhere and the exact spelling of every participating fragment.
fn island(
    document: &Document,
    parent: SourceId,
    encoded: &std::ops::Range<usize>,
) -> Option<std::ops::Range<usize>> {
    let source = &document.sources.get(parent.0)?.text;
    source.get(encoded.clone())?;
    if encoded.is_empty() {
        return None;
    }
    let mut indices = Vec::new();
    for (index, fragment) in document.fragments.iter().enumerate() {
        if let Provenance::Source { id, range } = &fragment.provenance
            && *id == parent
            && (range.start < encoded.end && encoded.start < range.end)
        {
            if range.is_empty() || source.get(range.clone())? != fragment.text {
                return None;
            }
            indices.push(index);
        }
    }
    let first = *indices.first()?;
    let last = *indices.last()?;
    if indices.len() != last - first + 1 {
        return None;
    }
    let mut end = None;
    for index in first..=last {
        let Provenance::Source { id, range } = &document.fragments[index].provenance else {
            return None;
        };
        if *id != parent || end.is_some_and(|end| end != range.start) {
            return None;
        }
        if index == first && range.start > encoded.start {
            return None;
        }
        end = Some(range.end);
    }
    let covered_start = match &document.fragments[first].provenance {
        Provenance::Source { range, .. } => range.start,
        Provenance::Generated => return None,
    };
    let covered_end = end?;
    for (index, fragment) in document.fragments.iter().enumerate() {
        if index >= first && index <= last {
            continue;
        }
        if matches!(&fragment.provenance, Provenance::Source { id, range }
            if *id == parent && (range.start < covered_end && covered_start < range.end))
        {
            return None;
        }
    }
    (covered_end >= encoded.end).then_some(first..last + 1)
}

fn slice(
    fragment: &document::Fragment,
    source: &str,
    range: std::ops::Range<usize>,
    role: Role,
) -> document::Fragment {
    let Provenance::Source { id, .. } = fragment.provenance else {
        unreachable!()
    };
    document::Fragment {
        text: source[range.clone()].to_owned(),
        role,
        provenance: Provenance::Source { id, range },
    }
}

fn replace(
    document: &mut Document,
    parent: SourceId,
    encoded: std::ops::Range<usize>,
    replacement: Vec<document::Fragment>,
) -> bool {
    let Some(indices) = island(document, parent, &encoded) else {
        return false;
    };
    let source = &document.sources[parent.0].text;
    let first = &document.fragments[indices.start];
    let last = &document.fragments[indices.end - 1];
    let Provenance::Source {
        range: first_range, ..
    } = &first.provenance
    else {
        return false;
    };
    let Provenance::Source {
        range: last_range, ..
    } = &last.provenance
    else {
        return false;
    };
    let mut fragments = Vec::new();
    if first_range.start < encoded.start {
        fragments.push(slice(
            first,
            source,
            first_range.start..encoded.start,
            first.role,
        ));
    }
    if !source[..encoded.start].ends_with('\n') {
        fragments.push(document::Fragment {
            text: "\n".to_owned(),
            role: Role::Whitespace,
            provenance: Provenance::Generated,
        });
    }
    for mut fragment in replacement {
        if fragment.role == Role::Literal {
            // Preserve the local parent role, not an unrelated prefix's role. A decoded
            // fragment spanning several parent roles cannot safely choose between them.
            let role = match &fragment.provenance {
                Provenance::Source { id, range } => document.sources[id.0].correspondence.iter()
                    .find(|mapping| mapping.decoded.start == range.start)
                    .and_then(|mapping| document.fragments[indices.clone()].iter().find(|parent| {
                        matches!(&parent.provenance, Provenance::Source { range, .. }
                            if range.start <= mapping.original.start && range.end >= mapping.original.end)
                    })).map(|parent| parent.role),
                Provenance::Generated => None,
            };
            fragment.role = role.unwrap_or(first.role);
        }
        fragments.push(fragment);
    }
    if encoded.end < last_range.end {
        fragments.push(slice(last, source, encoded.end..last_range.end, last.role));
    }
    document.fragments.splice(indices, fragments);
    true
}

// Syntax classification without formatting: only unchanged scalars with a certain direct
// correspondence may acquire child roles. Quotes, escapes, stripped tabs and continuations
// retain their shell roles and bytes. No generated geometry or decoded display is installed.
fn refine(
    document: &mut Document,
    parent: SourceId,
    decoded: SourceId,
    encoded: std::ops::Range<usize>,
    replacement: &[document::Fragment],
) -> bool {
    let Some(indices) = island(document, parent, &encoded) else {
        return false;
    };
    let source = &document.sources[parent.0].text;
    let child = &document.sources[decoded.0];
    let mut refinements: Vec<(std::ops::Range<usize>, Role)> = Vec::new();
    for fragment in replacement {
        let Provenance::Source { id, range } = &fragment.provenance else {
            continue;
        };
        if *id != decoded || fragment.role == Role::Literal {
            continue;
        }
        let start = child
            .correspondence
            .partition_point(|mapping| mapping.decoded.start < range.start);
        for mapping in child.correspondence[start..]
            .iter()
            .take_while(|mapping| mapping.decoded.start < range.end)
        {
            if mapping.decoded.end <= range.end
                && source.get(mapping.original.clone()) == child.text.get(mapping.decoded.clone())
            {
                if let Some((previous, role)) = refinements.last_mut()
                    && *role == fragment.role
                    && previous.end == mapping.original.start
                {
                    previous.end = mapping.original.end;
                    continue;
                }
                refinements.push((mapping.original.clone(), fragment.role));
            }
        }
    }
    if refinements
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start)
    {
        return false;
    }
    let mut fragments = Vec::new();
    for fragment in &document.fragments[indices.clone()] {
        let Provenance::Source { range, .. } = &fragment.provenance else {
            return false;
        };
        let mut offset = range.start;
        let start = refinements.partition_point(|(refined, _)| refined.end <= range.start);
        for (refined, role) in refinements[start..]
            .iter()
            .take_while(|(refined, _)| refined.start < range.end)
        {
            let refined = refined.start.max(range.start)..refined.end.min(range.end);
            if offset < refined.start {
                fragments.push(slice(
                    fragment,
                    source,
                    offset..refined.start,
                    fragment.role,
                ));
            }
            fragments.push(slice(fragment, source, refined.clone(), *role));
            offset = refined.end;
        }
        if offset < range.end {
            fragments.push(slice(fragment, source, offset..range.end, fragment.role));
        }
    }
    document.fragments.splice(indices, fragments);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_document() -> Document {
        let mut document = Document::original("pre'one\ntwo'suffix");
        assert!(document.push_source(SourceId(0), 0..5, Role::String));
        assert!(document.push_source(SourceId(0), 5..8, Role::Whitespace));
        assert!(document.push_source(SourceId(0), 8..18, Role::Comment));
        document
    }

    #[test]
    fn contiguous_split_island_preserves_boundary_roles_and_copy_mapping() {
        let mut document = split_document();
        let decoded = document
            .add_decoded("one\ntwo".to_owned(), SourceId(0), 4..11)
            .unwrap();
        let replacement = vec![document::Fragment {
            text: "one\ntwo".to_owned(),
            role: Role::String,
            provenance: Provenance::Source {
                id: decoded,
                range: 0..7,
            },
        }];
        assert!(replace(&mut document, SourceId(0), 4..11, replacement));
        assert_eq!(document.display_text(), "pre'\none\ntwo'suffix");
        assert_eq!(document.fragments.first().unwrap().role, Role::String);
        assert_eq!(document.fragments.last().unwrap().role, Role::Comment);
        assert_eq!(
            document.original_ranges(SourceId(0), 11..18),
            Some(std::iter::once(11..18).collect())
        );
        assert_eq!(document.execution_text(), "pre'one\ntwo'suffix");
    }

    #[test]
    fn rejected_composition_is_atomic_for_geometry_overlap_and_invalid_spelling() {
        for damage in 0..4 {
            let mut document = split_document();
            match damage {
                0 => document.fragments.insert(
                    1,
                    document::Fragment {
                        text: "\n".to_owned(),
                        role: Role::Whitespace,
                        provenance: Provenance::Generated,
                    },
                ),
                1 => {
                    assert!(document.push_source(SourceId(0), 6..9, Role::String));
                }
                2 => document.fragments[1].text = "wrong".to_owned(),
                _ => {
                    document.fragments[1].provenance = Provenance::Source {
                        id: SourceId(0),
                        range: 6..8,
                    }
                }
            }
            let before = document.clone();
            assert!(!replace(&mut document, SourceId(0), 4..11, Vec::new()));
            assert_eq!(document, before);
        }
    }

    #[test]
    fn literal_refinement_preserves_escapes_quotes_and_parent_roles() {
        let mut document = Document::original("'4\\2'");
        assert!(document.push_source(SourceId(0), 0..5, Role::String));
        let decoded = document
            .add_decoded_mapped(
                "42".to_owned(),
                SourceId(0),
                0..5,
                vec![
                    Correspondence {
                        decoded: 0..1,
                        original: 1..2,
                    },
                    Correspondence {
                        decoded: 1..2,
                        original: 2..4,
                    },
                ],
            )
            .unwrap();
        let replacement = vec![document::Fragment {
            text: "42".to_owned(),
            role: Role::Number,
            provenance: Provenance::Source {
                id: decoded,
                range: 0..2,
            },
        }];
        assert!(refine(
            &mut document,
            SourceId(0),
            decoded,
            0..5,
            &replacement
        ));
        assert_eq!(document.display_text(), "'4\\2'");
        assert_eq!(document.fragments[1].text, "4");
        assert_eq!(document.fragments[1].role, Role::Number);
        assert_eq!(document.fragments[2].role, Role::String);
        for fragment in &document.fragments {
            let Provenance::Source { id, range } = &fragment.provenance else {
                panic!("generated spelling");
            };
            assert_eq!(*id, SourceId(0));
            assert_eq!(
                document.original_ranges(*id, range.clone()),
                Some(vec![range.clone()])
            );
        }
    }

    #[test]
    fn formatting_disabled_keeps_exact_progressive_multiline_and_nested_spelling() {
        let policy = ShellOutputPresentationPolicy {
            enabled: false,
            ..Default::default()
        };
        for source in [
            "python3  -c 'value=42;\nprint(value)'  # tail",
            "node -e 'const value=42;\nprint(value); let result=('",
            "bash -c \"python3 -c 'value=42;print(value)'\"",
            "python3 -c \"print(\\\"value\\\");value=42\"",
        ] {
            for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
                let prefix = &source[..end];
                let document = present(prefix, &policy);
                assert_eq!(document.display_text(), prefix);
                assert_eq!(document.execution_text(), prefix);
                assert_eq!(document, present(prefix, &policy));
                assert!(document.fragments.iter().all(|fragment| matches!(
                    fragment.provenance,
                    Provenance::Source {
                        id: SourceId(0),
                        ..
                    }
                )));
                assert!(
                    document
                        .issues
                        .iter()
                        .all(|issue| issue.source.0 < document.sources.len())
                );
            }
        }
        #[cfg(feature = "embedded-python")]
        {
            let document = present("python3 -c 'value=42'", &policy);
            assert!(
                document
                    .fragments
                    .iter()
                    .any(|fragment| fragment.role == Role::Number)
            );
        }
    }

    #[test]
    fn failed_nested_admission_is_local_and_issue_uses_retained_parent() {
        let source = "echo before; bash -c 'echo child'; echo after";
        let policy = ShellOutputPresentationPolicy {
            max_depth: 1,
            ..Default::default()
        };
        let baseline = ShellOutputPresentationPolicy {
            embedded: false,
            ..policy
        };
        let document = present(source, &policy);
        assert_eq!(document.fragments, present(source, &baseline).fragments);
        assert_eq!(document.sources.len(), 1);
        assert_eq!(document.issues.len(), 1);
        assert_eq!(document.issues[0].source, SourceId(0));
        assert_eq!(document.issues[0].reason, AdapterFailure::BudgetExceeded);
        assert!(
            source
                .get(document.issues[0].range.clone())
                .unwrap()
                .contains("echo child")
        );
        assert_eq!(document, present(source, &policy));
    }

    #[test]
    fn nested_failure_issue_survives_successful_parent_composition() {
        let source = "bash -c \"bash -c 'echo child'\"";
        let policy = ShellOutputPresentationPolicy {
            max_depth: 2,
            ..Default::default()
        };
        let document = present(source, &policy);
        assert_eq!(document.sources.len(), 2);
        assert_eq!(document.issues.len(), 1);
        let issue = &document.issues[0];
        assert_eq!(issue.source, SourceId(1));
        assert_eq!(issue.reason, AdapterFailure::BudgetExceeded);
        assert!(
            document.sources[issue.source.0]
                .text
                .get(issue.range.clone())
                .unwrap()
                .contains("echo child")
        );
        assert_eq!(document.execution_text(), source);
    }

    #[test]
    fn bool_adapter_failure_rolls_back_locally_without_dangling_issue_sources() {
        struct Reject;
        impl LanguageAdapter for Reject {
            fn present(&self, _: &mut Document, _: &AdapterInput, _: &Budget) -> bool {
                false
            }
        }
        let mut document = split_document();
        let before = document.clone();
        let checkpoint = (
            document.fragments.clone(),
            document.sources.len(),
            document.issues.len(),
        );
        let id = document
            .add_decoded("one\ntwo".to_owned(), SourceId(0), 4..11)
            .unwrap();
        let input = AdapterInput {
            source: id,
            range: 0..7,
            depth: 1,
        };
        let reason = Reject
            .present_outcome(&mut document, &input, &Budget::default())
            .unwrap_err();
        assert_eq!(reason, AdapterFailure::Unavailable);
        rollback(
            &mut document,
            checkpoint,
            PresentationIssue {
                source: SourceId(0),
                range: 4..11,
                reason,
            },
        );
        assert_eq!(document.fragments, before.fragments);
        assert_eq!(document.sources, before.sources);
        assert_eq!(document.issues[0].source, SourceId(0));
        assert_eq!(document.issues[0].range, 4..11);
    }

    #[test]
    fn disabled_language_and_dynamic_inputs_keep_literal_spelling_without_decoding() {
        let policy = ShellOutputPresentationPolicy {
            enabled: false,
            python: false,
            javascript: false,
            nested_shell: false,
            ..Default::default()
        };
        for source in [
            "python3 -c 'value=42'",
            "node -e 'const value=42'",
            "bash -c 'echo yes'",
            "python3 -c \"print($value)\"",
        ] {
            let document = present(source, &policy);
            assert_eq!(document.display_text(), source);
            assert_eq!(document.sources.len(), 1);
        }
    }

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
                            if matches!(
                                document.sources[id.0].origin,
                                document::SourceOrigin::Formatted { .. }
                            ) {
                                assert_eq!(document.original_ranges(*id, range.clone()), None);
                                assert!(document.original_region(*id).is_some());
                            } else {
                                let ranges = document.original_ranges(*id, range.clone()).unwrap();
                                assert!(
                                    ranges
                                        .iter()
                                        .all(|range| prefix.get(range.clone()).is_some())
                                );
                            }
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
