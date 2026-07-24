#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::{
    ShellAnalysisCompleteness, ShellAnalysisRequest, ShellIncompleteReason,
};
use proptest::prelude::*;

fn command_count(source: &str) -> usize {
    analyze(&ShellAnalysisRequest::posix(source))
        .expect("generated shell source should parse")
        .commands
        .len()
}

proptest! {
    #[test]
    fn quoted_separators_do_not_add_subjects(text in "[a-zA-Z0-9_]{0,32}", separator in prop_oneof![Just("|"), Just(";")]) {
        let source = format!("printf '%s' '{text}{separator}{text}'");
        prop_assert_eq!(command_count(&source), 1);
    }

    #[test]
    fn real_separators_add_subjects(left in "[a-z]{1,12}", right in "[a-z]{1,12}", separator in prop_oneof![Just(";"), Just("\n"), Just(" & "), Just(" | "), Just(" && "), Just(" || ")]) {
        let source = format!("printf {left}{separator}printf {right}");
        prop_assert_eq!(command_count(&source), 2);
    }

    #[test]
    fn harmless_whitespace_preserves_subjects(spaces in 1usize..8) {
        let source = format!("printf{}'%s'{}ok", " ".repeat(spaces), " ".repeat(spaces));
        let analysis = analyze(&ShellAnalysisRequest::posix(source)).unwrap();
        prop_assert_eq!(analysis.commands.len(), 1);
        let executable = &analysis.commands[0].executable;
        let is_printf = matches!(executable, bcode_shell_command_analysis_models::ShellWord::Static { value, .. } if value == "printf");
        prop_assert!(is_printf);
    }

    #[test]
    fn source_spans_are_utf8_boundaries(prefix in "[éλ🦀a-z]{0,12}") {
        let source = format!("printf '%s' '{prefix}'; cat input.txt");
        let analysis = analyze(&ShellAnalysisRequest::posix(&source)).unwrap();
        for command in analysis.commands {
            let start = command.span.start as usize;
            let end = command.span.end as usize;
            prop_assert!(source.is_char_boundary(start));
            prop_assert!(source.is_char_boundary(end));
            prop_assert_eq!(&source[start..end], command.source);
        }
    }
}

#[test]
fn parser_errors_never_produce_analysis() {
    assert!(analyze(&ShellAnalysisRequest::posix("if true; then")).is_err());
}

#[test]
fn heredoc_bodies_never_become_commands() {
    let analysis = analyze(&ShellAnalysisRequest::posix(
        "python3 - <<'PY'\nrm -rf /; denied | command\nPY\n",
    ))
    .unwrap();
    assert_eq!(analysis.commands.len(), 1);
    assert_eq!(analysis.commands[0].source, "python3 - <<'PY'");
}

#[test]
fn dynamic_command_names_are_incomplete() {
    let analysis = analyze(&ShellAnalysisRequest::posix("cmd=printf; \"$cmd\" ok")).unwrap();
    assert!(matches!(
        analysis.completeness,
        ShellAnalysisCompleteness::Incomplete { ref reasons }
            if reasons.iter().any(|reason| matches!(reason, ShellIncompleteReason::DynamicExecutable { .. }))
    ));
}

#[test]
fn execution_capable_extension_variants_fail_closed() {
    for source in [
        "name() { printf ok; }",
        "for ((i=0; i<1; i++)); do printf ok; done",
        "cat <(printf ok)",
        "cat <<< data",
    ] {
        let result = analyze(&ShellAnalysisRequest::posix(source));
        assert!(
            result.is_err() || result.is_ok_and(|analysis| !analysis.completeness.is_complete()),
            "extension unexpectedly completed: {source}"
        );
    }
}
