//! Conservative possible restrictions for legacy source globs with expanded arguments.
//!
//! Source globs remain source globs (not the argv DSL). A literal prefix can prove a
//! restriction disjoint; complex glob suffixes are deliberately not used as allow proofs.

use super::{Action, Rule, compiled_commands::CompiledCommandLayer, glob_matches};
use bcode_shell_command_analysis_models::{ShellCommand, ShellWord};

pub fn restrictions<'a>(
    layer: &'a CompiledCommandLayer,
    command: &ShellCommand,
) -> impl Iterator<Item = (usize, &'a Rule)> {
    let mut prefix = String::new();
    let mut dynamic = false;
    for word in std::iter::once(&command.executable).chain(&command.arguments) {
        match word {
            ShellWord::Static { value, .. } => {
                if !prefix.is_empty() {
                    prefix.push(' ');
                }
                prefix.push_str(value);
            }
            ShellWord::Dynamic { .. } => {
                prefix.push(' ');
                dynamic = true;
                break;
            }
        }
    }
    layer
        .legacy_candidates(&prefix)
        .filter(|(_, rule)| {
            if !dynamic
                || rule.action == Action::Allow
                || command
                    .match_candidates
                    .iter()
                    .any(|candidate| glob_matches(&rule.pattern, &candidate.subject))
            {
                return false;
            }
            let literal = rule
                .pattern
                .split(['*', '?', '[', '{', '\\'])
                .next()
                .unwrap_or_default();
            prefix.starts_with(literal) || literal.starts_with(&prefix)
        })
        .collect::<Vec<_>>()
        .into_iter()
}
