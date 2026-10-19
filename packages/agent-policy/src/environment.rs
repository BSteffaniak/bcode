//! Environment prefixes are operation facts, not an implicit permission action.

use crate::shell_subject::{Subject, literal as static_value, validate_executable};
use bcode_shell_command_analysis_models::{ShellCommand, ShellWord};
use std::borrow::Cow;

/// Normalize reviewed env prefixes without cloning or draining argument vectors.
pub fn effective_command(command: &ShellCommand) -> Result<Subject<'_>, &'static str> {
    let mut subject = Subject::original(command);
    let mut changed = !command.assignments.is_empty();
    while matches!(
        static_value(&subject.executable),
        Some("env" | "/usr/bin/env" | "/bin/env")
    ) {
        let utility = utility(subject.arguments)?;
        let index = utility.executable_index;
        let Some(executable) = subject.arguments.get(index) else {
            break;
        };
        subject.has_assignments |= utility.has_assignments;
        subject.executable = Cow::Borrowed(executable);
        subject.arguments = &subject.arguments[index + 1..];
        changed = true;
    }
    validate_executable(&subject.executable)?;
    if changed {
        if let Some(offset) = subject
            .executable
            .span()
            .start
            .checked_sub(command.span.start)
            && let Some(source) = command.source.get(offset as usize..)
        {
            subject.add_candidate(Cow::Borrowed(source));
        }
        subject.add_canonical();
    }
    Ok(subject)
}

pub struct Utility {
    pub executable_index: usize,
    pub has_assignments: bool,
}

/// Parse the reviewed env launcher grammar shared by effective commands and restriction views.
pub fn utility(arguments: &[ShellWord]) -> Result<Utility, &'static str> {
    let mut index = 0;
    let mut options = true;
    let mut has_assignments = false;
    while let Some(word) = arguments.get(index) {
        let value = static_value(word).ok_or("env command prefix or executable is dynamic")?;
        if options && value == "--" {
            options = false;
        } else if options && matches!(value, "-i" | "--ignore-environment" | "-" | "-0" | "--null")
        {
            // These change the environment or its display, not the utility's policy identity.
        } else if options && matches!(value, "-u" | "--unset" | "-C" | "--chdir") {
            index += 1;
            if arguments.get(index).and_then(static_value).is_none() {
                return Err("env option requires a static value");
            }
        } else if options && (value.starts_with("--unset=") || value.starts_with("--chdir=")) {
            // The option value occupies the same argv word.
        } else if options && value.starts_with('-') {
            return Err("unsupported env option; effective command cannot be determined");
        } else if assignment(value).is_some() {
            has_assignments = true;
            options = false;
        } else {
            break;
        }
        index += 1;
    }
    Ok(Utility {
        executable_index: index,
        has_assignments,
    })
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
