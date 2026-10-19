//! Bounded symbolic execution of the existing argv-pattern NFA.

use crate::shell_subject::CommandWords;

use super::{Atom, MAX_WORD_BYTES, MAX_WORDS, WordRule};
use bcode_shell_command_analysis_models::{ShellArgumentShape, ShellWord};
use std::collections::{BTreeSet, VecDeque};

type States = BTreeSet<(usize, bool)>;
const MAX_WORK: usize = 262_144;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    Definite,
    Possible,
    Absent,
}

#[cfg(test)]
thread_local! { pub static CLASSIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub fn classify(rule: &WordRule, command: &impl CommandWords) -> Match {
    #[cfg(test)]
    CLASSIFICATIONS.with(|count| count.set(count.get() + 1));
    let words = std::iter::once(command.executable())
        .chain(command.arguments())
        .collect::<Vec<_>>();
    if words.len() > MAX_WORDS
        || words
            .iter()
            .map(|word| match word {
                ShellWord::Static { value, .. } => value.len(),
                ShellWord::Dynamic {
                    shape: ShellArgumentShape::One { prefix, suffix },
                    ..
                } => prefix.len().saturating_add(suffix.len()),
                ShellWord::Dynamic { .. } => 0,
            })
            .sum::<usize>()
            > MAX_WORD_BYTES
    {
        return Match::Possible;
    }
    if definite(rule, &words) {
        Match::Definite
    } else if possible(rule, &words) {
        Match::Possible
    } else {
        Match::Absent
    }
}

fn closure(rule: &WordRule, states: &mut States) {
    let mut queue = VecDeque::from_iter(states.iter().copied());
    while let Some((state, ended)) = queue.pop_front() {
        for (atom, target) in &rule.edges[state] {
            if atom.is_none() && states.insert((*target, ended)) {
                queue.push_back((*target, ended));
            }
        }
    }
}

const fn option_atom(atom: &Atom) -> bool {
    matches!(
        atom,
        Atom::Option | Atom::OptionWithValue | Atom::OptionValue | Atom::OptionsEnd
    )
}

fn step(rule: &WordRule, states: &States, word: Option<&str>) -> States {
    let mut next = States::new();
    for &(state, ended) in states {
        for (atom, target) in &rule.edges[state] {
            let Some(atom) = atom else {
                continue;
            };
            if ended && option_atom(atom) {
                continue;
            }
            // Unknown values are guaranteed to match only the DSL's arbitrary-value atom.
            if word.map_or_else(|| matches!(atom, Atom::Any), |value| atom.accepts(value)) {
                next.insert((*target, ended || word.is_none_or(|value| value == "--")));
            }
        }
    }
    closure(rule, &mut next);
    next
}

fn definite(rule: &WordRule, words: &[&ShellWord]) -> bool {
    let mut initial = States::from([(0, false)]);
    closure(rule, &mut initial);
    let mut queue = VecDeque::from([(0, initial)]);
    let mut seen = BTreeSet::new();
    let mut budget = 0;
    while let Some((index, states)) = queue.pop_front() {
        budget += states.len().saturating_mul(rule.edges.len());
        if budget > MAX_WORK || states.is_empty() {
            return false;
        }
        if !seen.insert((index, states.clone())) {
            continue;
        }
        let Some(word) = words.get(index) else {
            if !states.iter().any(|(state, _)| *state == rule.accept) {
                return false;
            }
            continue;
        };
        match word {
            ShellWord::Static { value, .. } => {
                queue.push_back((index + 1, step(rule, &states, Some(value))));
            }
            ShellWord::Dynamic {
                shape: ShellArgumentShape::One { .. },
                ..
            } => queue.push_back((index + 1, step(rule, &states, None))),
            ShellWord::Dynamic {
                shape: ShellArgumentShape::Many,
                ..
            } => {
                queue.push_back((index + 1, states.clone()));
                queue.push_back((index, step(rule, &states, None)));
            }
        }
    }
    true
}

fn could_match(atom: &Atom, word: &ShellWord) -> bool {
    match word {
        ShellWord::Static { value, .. } => atom.accepts(value),
        ShellWord::Dynamic {
            shape: ShellArgumentShape::One { prefix, suffix },
            ..
        } => match atom {
            Atom::Literal(value) => {
                value.starts_with(prefix)
                    && value.ends_with(suffix)
                    && value.len() >= prefix.len().saturating_add(suffix.len())
            }
            _ => true, // Regex/option intersections are conservative, never permission grants.
        },
        ShellWord::Dynamic { .. } => true,
    }
}

fn possible(rule: &WordRule, words: &[&ShellWord]) -> bool {
    let mut queue = VecDeque::from([(0, 0, false)]);
    let mut seen = BTreeSet::new();
    let mut budget = 0;
    while let Some((state, index, ended)) = queue.pop_front() {
        if !seen.insert((state, index, ended)) {
            continue;
        }
        budget += rule.edges[state].len() + 1;
        if budget > MAX_WORK {
            return true;
        }
        if state == rule.accept && index == words.len() {
            return true;
        }
        let word = words.get(index).copied();
        let many = matches!(
            word,
            Some(ShellWord::Dynamic {
                shape: ShellArgumentShape::Many,
                ..
            })
        );
        if many {
            queue.push_back((state, index + 1, ended));
        }
        for (atom, target) in &rule.edges[state] {
            let Some(atom) = atom else {
                queue.push_back((*target, index, ended));
                continue;
            };
            if ended && option_atom(atom) {
                continue;
            }
            if let Some(word) = word
                && could_match(atom, word)
            {
                let next = index + usize::from(!many);
                match word {
                    ShellWord::Static { value, .. } => {
                        queue.push_back((*target, next, ended || value == "--"));
                    }
                    ShellWord::Dynamic { .. } => {
                        queue.push_back((*target, next, ended));
                        queue.push_back((*target, next, true));
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word_patterns::{matches, parse};
    use bcode_agent_policy_models::Action;
    use bcode_shell_command_analysis::analyze;
    use bcode_shell_command_analysis_models::ShellAnalysisRequest;

    #[test]
    fn symbolic_classification_is_sound_for_concrete_expansion_witnesses() {
        for pattern in [
            "cmd <arg>",
            "cmd ...",
            "cmd /a.*/",
            "cmd [a | b c] ...",
            "cmd <options>* end",
            "cmd [--] <arg>",
        ] {
            let rule = parse(pattern, Action::Allow).unwrap();
            for source in [
                "cmd \"$value\"",
                "cmd $value",
                "cmd \"pre-$value-end\"",
                "cmd \"$@\"",
                "cmd $value end",
            ] {
                let analysis = analyze(&ShellAnalysisRequest::posix(source)).unwrap();
                let command = &analysis.commands[0];
                let result = classify(&rule, command);
                for values in [
                    vec![],
                    vec![""],
                    vec!["a"],
                    vec!["b", "c"],
                    vec!["--"],
                    vec!["-x", "end"],
                    vec!["dangerous"],
                ] {
                    let mut concrete = vec!["cmd".to_owned()];
                    for argument in &command.arguments {
                        match argument {
                            ShellWord::Static { value, .. } => concrete.push(value.clone()),
                            ShellWord::Dynamic {
                                shape: ShellArgumentShape::One { prefix, suffix },
                                ..
                            } => concrete.push(format!("{prefix}{}{suffix}", values.join(" "))),
                            ShellWord::Dynamic { .. } => {
                                concrete.extend(values.iter().map(ToString::to_string));
                            }
                        }
                    }
                    let argv = concrete
                        .iter()
                        .map(|value| Some(value.as_str()))
                        .collect::<Vec<_>>();
                    let accepts = matches(&rule, &argv, false);
                    if result == Match::Definite {
                        assert!(accepts, "{pattern}: {source}: {concrete:?}");
                    }
                    if result == Match::Absent {
                        assert!(!accepts, "{pattern}: {source}: {concrete:?}");
                    }
                }
            }
        }
    }
}
