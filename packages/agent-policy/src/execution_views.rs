//! Restriction-only execution views derived from parsed shell words.
//!
//! These views never grant an allow: original subjects remain authoritative for
//! grants, and original spans are retained for diagnostics. Paths are compared
//! by basename only for restrictions, never as proof of executable identity.

use crate::shell_subject::{Subject, validate_executable};
use bcode_shell_command_analysis_models::ShellWord;

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
        let index = crate::environment::utility(arguments)
            .map_err(str::to_owned)?
            .executable_index;
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

// A work allowance, not a wrapper-depth limit. Source/argv scans across all
// views are charged before allocating canonical candidates or matching rules.
const MAX_VIEW_WORK: usize = 1024 * 1024;

struct ViewWork(usize);

impl ViewWork {
    fn charge(
        &mut self,
        subject: &Subject<'_>,
        executable: &ShellWord,
        arguments: &[ShellWord],
    ) -> Result<(), String> {
        let offset = executable.span().start.saturating_sub(subject.span.start) as usize;
        let work = subject
            .source
            .len()
            .saturating_sub(offset)
            .saturating_add(arguments.len());
        self.0 = self
            .0
            .checked_sub(work)
            .ok_or("shell execution views exceed work allowance")?;
        Ok(())
    }
}

/// Visit one borrowed view at a time. No suffix commands survive the callback.
pub fn visit(subject: &Subject<'_>, mut visitor: impl FnMut(&Subject<'_>)) -> Result<(), String> {
    let mut executable = subject.executable.as_ref();
    let mut arguments = subject.arguments;
    let mut first = true;
    let mut work = ViewWork(MAX_VIEW_WORK);
    loop {
        validate_executable(executable).map_err(str::to_owned)?;
        let value = literal(executable).expect("validated executable");
        let name = basename(value);
        if name != value || (first && subject.has_assignments) {
            work.charge(subject, executable, arguments)?;
            let view = Subject::restriction(subject, executable, arguments, name != value);
            validate_executable(&view.executable).map_err(str::to_owned)?;
            visitor(&view);
        }
        first = false;
        if !matches!(name, "env" | "command" | "exec") {
            break;
        }
        let Some(index) = launched_word(name, arguments)? else {
            break;
        };
        executable = &arguments[index];
        arguments = &arguments[index + 1..];
        validate_executable(executable).map_err(str::to_owned)?;
        work.charge(subject, executable, arguments)?;
        visitor(&Subject::restriction(subject, executable, arguments, false));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_shell_command_analysis::analyze;
    use bcode_shell_command_analysis_models::ShellAnalysisRequest;

    #[test]
    fn restriction_views_borrow_original_argv_and_source() {
        let analysis = analyze(&ShellAnalysisRequest::posix(
            "MODE=test command env -i /usr/bin/git status",
        ))
        .unwrap();
        let command = &analysis.commands[0];
        let subject = crate::environment::effective_command(command).unwrap();
        let mut visits = 0;
        visit(&subject, |view| {
            visits += 1;
            let end = command.arguments.as_ptr_range().end;
            assert_eq!(view.arguments.as_ptr_range().end, end);
            assert_eq!(
                view.source.as_bytes().as_ptr_range().end,
                command.source.as_bytes().as_ptr_range().end
            );
        })
        .unwrap();
        assert_eq!(visits, 4);
    }
}
