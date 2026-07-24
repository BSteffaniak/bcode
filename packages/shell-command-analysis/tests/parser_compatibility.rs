#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use brush_parser::{Parser, ParserOptions};
use serde::Deserialize;
use std::io::Cursor;

#[derive(Debug, Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    source: String,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    commands: Vec<String>,
    completeness: String,
}

fn parse(source: &str) -> Result<(), String> {
    let options = ParserOptions {
        enable_extended_globbing: false,
        posix_mode: true,
        sh_mode: true,
        tilde_expansion_at_word_start: true,
        tilde_expansion_after_colon: false,
        ..ParserOptions::default()
    };
    Parser::new(Cursor::new(source.as_bytes()), &options)
        .parse_program()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[test]
fn brush_parser_compatibility_spike_covers_corpus() {
    let corpus: Corpus = serde_json::from_str(include_str!(
        "../../../fixtures/shell-command-analysis/corpus.json"
    ))
    .unwrap();
    let mut disagreements = Vec::new();
    for case in corpus.cases {
        let result = parse(&case.source);
        let should_parse =
            case.expected.completeness != "error" && case.id != "process-substitution-leaf";
        if should_parse != result.is_ok() {
            disagreements.push(format!("{}: {result:?}", case.id));
            continue;
        }
        if should_parse {
            match analyze(&ShellAnalysisRequest::posix(&case.source)) {
                Ok(analysis) => {
                    let expected_count = case.expected.commands.len();
                    if analysis.commands.len() != expected_count {
                        disagreements.push(format!(
                            "{}: expected {expected_count} commands, extracted {}",
                            case.id,
                            analysis.commands.len()
                        ));
                    }
                }
                Err(error) => disagreements.push(format!("{}: adapter error: {error:?}", case.id)),
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "brush-parser compatibility disagreements:\n{}",
        disagreements.join("\n")
    );
}
