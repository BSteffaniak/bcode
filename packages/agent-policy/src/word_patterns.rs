//! Parsed, command-independent shell word permission patterns.

use bcode_agent_policy_models::Action;
use bcode_shell_command_analysis_models::{ShellCommand, ShellWord};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Optional(String),
    Rest,
}

#[derive(Debug, Clone)]
pub struct WordRule {
    pub pattern: String,
    pub action: Action,
    segments: Vec<Segment>,
    specificity: (usize, usize),
}

fn parse(pattern: &str) -> Result<Vec<Segment>, String> {
    if pattern.len() > 1024 {
        return Err("word pattern exceeds 1024 bytes".to_owned());
    }
    let mut segments = Vec::new();
    for (index, token) in pattern.split_whitespace().enumerate() {
        let segment = if token == "..." {
            Segment::Rest
        } else if token.starts_with('[') || token.ends_with(']') {
            let Some(inner) = token.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
                return Err(format!("unbalanced optional word in {pattern:?}"));
            };
            if inner.is_empty()
                || inner.contains(['[', ']', '*', '\\', '\'', '"'])
                || inner == "..."
            {
                return Err(format!("invalid optional word in {pattern:?}"));
            }
            Segment::Optional(inner.to_owned())
        } else {
            if token.contains(['[', ']', '*', '\\', '\'', '"']) || token == "..." {
                return Err(format!("invalid literal word in {pattern:?}"));
            }
            Segment::Literal(token.to_owned())
        };
        if matches!(segment, Segment::Rest) && index == 0 {
            return Err("a word pattern must start with a literal executable".to_owned());
        }
        segments.push(segment);
    }
    if !matches!(segments.first(), Some(Segment::Literal(_)))
        || segments[..segments.len().saturating_sub(1)].contains(&Segment::Rest)
    {
        return Err(format!("invalid word pattern {pattern:?}"));
    }
    if segments.len() > 32 {
        return Err("word pattern exceeds 32 segments".to_owned());
    }
    Ok(segments)
}

pub fn compile(rules: &BTreeMap<String, Action>) -> Result<Vec<WordRule>, String> {
    rules
        .iter()
        .map(|(pattern, action)| {
            let segments = parse(pattern)?;
            let specificity = (
                segments
                    .iter()
                    .filter(|s| matches!(s, Segment::Literal(_)))
                    .count(),
                segments
                    .iter()
                    .filter(|s| matches!(s, Segment::Optional(_)))
                    .count(),
            );
            Ok(WordRule {
                pattern: pattern.clone(),
                action: *action,
                segments,
                specificity,
            })
        })
        .collect()
}

fn static_word(word: &ShellWord) -> Option<&str> {
    match word {
        ShellWord::Static { value, .. } => Some(value),
        ShellWord::Dynamic { .. } => None,
    }
}

fn matches(segments: &[Segment], words: &[Option<&str>], uncertain: bool) -> bool {
    let mut states = vec![false; words.len() + 1];
    states[0] = true;
    for segment in segments {
        let mut next = vec![false; words.len() + 1];
        for (index, matched) in states.iter().enumerate() {
            if !matched {
                continue;
            }
            match segment {
                Segment::Literal(value)
                    if words.get(index).is_some_and(|word| {
                        *word == Some(value.as_str()) || (uncertain && word.is_none())
                    }) =>
                {
                    next[index + 1] = true;
                }
                Segment::Optional(value) => {
                    next[index] = true;
                    if words.get(index).is_some_and(|word| {
                        *word == Some(value.as_str()) || (uncertain && word.is_none())
                    }) {
                        next[index + 1] = true;
                    }
                }
                Segment::Rest => {
                    next[index] = true;
                    for end in index..words.len() {
                        if !uncertain && words[end].is_none() {
                            break;
                        }
                        next[end + 1] = true;
                    }
                }
                Segment::Literal(_) => {}
            }
        }
        states = next;
    }
    states[words.len()]
}

pub fn matching<'a>(rules: &'a [WordRule], command: &ShellCommand) -> Option<&'a WordRule> {
    if !command.assignments.is_empty() {
        return None;
    }
    let words = std::iter::once(&command.executable)
        .chain(&command.arguments)
        .map(static_word)
        .collect::<Vec<_>>();
    if words.first().is_none_or(Option::is_none) {
        return None;
    }
    rules
        .iter()
        .filter(|rule| matches(&rule.segments, &words, false))
        .max_by(|lhs, rhs| {
            action_priority(lhs.action)
                .cmp(&action_priority(rhs.action))
                .then_with(|| lhs.specificity.cmp(&rhs.specificity))
                .then_with(|| rhs.pattern.cmp(&lhs.pattern))
        })
}

/// A deny that may apply once dynamically expanded words are known.
pub fn uncertain_deny<'a>(rules: &'a [WordRule], command: &ShellCommand) -> Option<&'a WordRule> {
    let words = std::iter::once(&command.executable)
        .chain(&command.arguments)
        .map(static_word)
        .collect::<Vec<_>>();
    if words.iter().all(Option::is_some) {
        return None;
    }
    rules
        .iter()
        .filter(|rule| rule.action == Action::Deny && matches(&rule.segments, &words, true))
        .max_by_key(|rule| rule.specificity)
}

const fn action_priority(action: Action) -> u8 {
    match action {
        Action::Allow => 0,
        Action::Ask => 1,
        Action::Deny => 2,
    }
}
