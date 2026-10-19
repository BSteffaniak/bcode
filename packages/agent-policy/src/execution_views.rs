//! Restriction-only execution views derived from parsed shell words.
//!
//! These views never grant an allow: original subjects remain authoritative for
//! grants, and original spans are retained for diagnostics. Paths are compared
//! by basename only for restrictions, never as proof of executable identity.

use bcode_shell_command_analysis_models::{
    ShellCommand, ShellCommandMatchCandidate, ShellCommandMatchCandidateKind, ShellWord,
};

fn literal(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

/// Resolve only reviewed launcher syntax. Unknown/dynamic launcher options must
/// not turn a configured denial into an allow.
fn launched_word(executable: &str, arguments: &[ShellWord]) -> Result<Option<usize>, String> {
    if executable == "env" {
        let (index, _) = crate::environment::utility(arguments).map_err(str::to_owned)?;
        return Ok((index < arguments.len()).then_some(index));
    }
    let mut index = 0;
    while let Some(word) = arguments.get(index) {
        let value = literal(word).ok_or("dynamic launcher argument")?;
        if value == "--" {
            index += 1;
            break;
        }
        match executable {
            "command" => {
                if matches!(value, "-v" | "-V") {
                    return Ok(None); // Query only: it does not execute its operand.
                }
                if value == "-p" {
                    index += 1;
                    continue;
                }
                if value.starts_with('-') {
                    return Err("unsupported command launcher option".to_owned());
                }
            }
            "exec" => {
                if matches!(value, "-c" | "-l" | "-cl" | "-lc") {
                    index += 1;
                    continue;
                }
                if value == "-a" {
                    index += 1;
                    if arguments.get(index).and_then(literal).is_none() {
                        return Err("missing or dynamic exec name".to_owned());
                    }
                    index += 1;
                    continue;
                }
                if value.starts_with('-') {
                    return Err("unsupported exec launcher option".to_owned());
                }
            }
            _ => return Ok(None),
        }
        break;
    }
    Ok((index < arguments.len()).then_some(index))
}

pub fn views(subject: &ShellCommand) -> Result<Vec<ShellCommand>, String> {
    let mut current = subject.clone();
    let mut views = Vec::new();
    loop {
        let executable = literal(&current.executable).ok_or("dynamic launched executable")?;
        let name = basename(executable).to_owned();
        let changed = name != executable || !current.assignments.is_empty();
        if changed {
            current.executable = ShellWord::Static {
                value: name.clone(),
                span: current.executable.span(),
            };
            current.assignments.clear();
            update_candidates(&mut current, subject);
            views.push(current.clone());
        }
        if !matches!(name.as_str(), "env" | "command" | "exec") {
            break;
        }
        let Some(index) = launched_word(&name, &current.arguments)? else {
            break;
        };
        current.executable = current.arguments[index].clone();
        current.arguments.drain(..=index);
        update_candidates(&mut current, subject);
        views.push(current.clone());
    }
    Ok(views)
}

fn update_candidates(view: &mut ShellCommand, original: &ShellCommand) {
    view.match_candidates.clear();
    let offset = usize::try_from(
        view.executable
            .span()
            .start
            .saturating_sub(original.span.start),
    )
    .unwrap_or(usize::MAX);
    if let Some(source) = original.source.get(offset..) {
        source.clone_into(&mut view.source);
        view.match_candidates.push(ShellCommandMatchCandidate {
            subject: source.to_owned(),
            kind: ShellCommandMatchCandidateKind::Original,
            transformation: None,
        });
    }
    let words = std::iter::once(&view.executable).chain(&view.arguments);
    if let Some(words) = words.map(literal).collect::<Option<Vec<_>>>()
        && words
            .iter()
            .all(|word| !word.is_empty() && !word.chars().any(char::is_whitespace))
    {
        view.match_candidates.push(ShellCommandMatchCandidate {
            subject: words.join(" "),
            kind: ShellCommandMatchCandidateKind::Canonical,
            transformation: Some("restriction-only execution view".to_owned()),
        });
    }
}
