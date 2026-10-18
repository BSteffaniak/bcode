//! Environment prefixes are operation facts, not an implicit permission action.

use std::borrow::Cow;

use bcode_shell_command_analysis_models::{
    ShellAssignment, ShellCommand, ShellCommandMatchCandidate, ShellCommandMatchCandidateKind,
    ShellWord,
};

/// Retain the original subject for environment-sensitive globs while matching ordinary rules
/// against the executable and argv. Unsupported `env` options must not hide an executable.
pub fn effective_command(command: &ShellCommand) -> Result<Cow<'_, ShellCommand>, &'static str> {
    let mut effective = Cow::Borrowed(command);
    while matches!(
        static_value(&effective.executable),
        Some("env" | "/usr/bin/env" | "/bin/env")
    ) {
        let mut index = 0;
        let mut options = true;
        let mut assignments = Vec::new();
        while let Some(word) = effective.arguments.get(index) {
            let Some(value) = static_value(word) else {
                return Err("env command prefix or executable is dynamic");
            };
            if options && value == "--" {
                options = false;
            } else if options && matches!(value, "-i" | "--ignore-environment" | "-") {
                // These change the environment, not the command's policy identity.
            } else if options && matches!(value, "-u" | "--unset") {
                index += 1;
                if effective
                    .arguments
                    .get(index)
                    .and_then(static_value)
                    .is_none()
                {
                    return Err("env unset option requires a static variable name");
                }
            } else if options && value.starts_with("--unset=") {
                // No additional argv word.
            } else if options && value.starts_with('-') {
                return Err("unsupported env option; effective command cannot be determined");
            } else if let Some((name, value)) = assignment(value) {
                assignments.push(ShellAssignment {
                    name: name.to_owned(),
                    value: ShellWord::Static {
                        value: value.to_owned(),
                        span: word.span(),
                    },
                    span: word.span(),
                });
                options = false;
            } else {
                break;
            }
            index += 1;
        }
        let Some(executable) = effective.arguments.get(index).cloned() else {
            // With no utility, env itself prints the environment.
            break;
        };
        let effective = effective.to_mut();
        effective.arguments.drain(..=index);
        effective.executable = executable;
        effective.assignments.extend(assignments);
    }
    if matches!(
        static_value(&effective.executable),
        Some("eval" | "." | "source")
    ) {
        return Err("effective command contains unresolved shell source");
    }
    if effective.assignments.is_empty() && matches!(effective, Cow::Borrowed(_)) {
        return Ok(effective);
    }
    let effective = effective.to_mut();
    // Keep original source and byte spans. This additional subject is only a policy candidate.
    if let Some(offset) = effective
        .executable
        .span()
        .start
        .checked_sub(command.span.start)
        && let Some(source) = command.source.get(offset as usize..)
    {
        add_candidate(effective, source.to_owned());
    }
    let words = std::iter::once(&effective.executable)
        .chain(&effective.arguments)
        .map(static_value)
        .collect::<Option<Vec<_>>>();
    if let Some(words) = words
        && words
            .iter()
            .all(|word| !word.is_empty() && !word.chars().any(char::is_whitespace))
    {
        add_candidate(effective, words.join(" "));
    }
    Ok(Cow::Owned(effective.clone()))
}

fn add_candidate(command: &mut ShellCommand, subject: String) {
    if !command
        .match_candidates
        .iter()
        .any(|candidate| candidate.subject == subject)
    {
        command.match_candidates.push(ShellCommandMatchCandidate {
            subject,
            kind: ShellCommandMatchCandidateKind::Canonical,
            transformation: Some("effective command with environment retained as facts".to_owned()),
        });
    }
}

fn static_value(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

fn assignment(value: &str) -> Option<(&str, &str)> {
    let (name, value) = value.split_once('=')?;
    let mut chars = name.chars();
    (chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()))
    .then_some((name, value))
}
