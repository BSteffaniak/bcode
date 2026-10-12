//! Bounded grammatical evidence, never a prose summary or resolved task label.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_CLAUSES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Purpose,
    Step,
    Prohibition,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Task {
    Implementation,
    Debugging,
    Review,
    Testing,
    Documentation,
    Planning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Domain {
    Frontend,
    Backend,
    Persistence,
    Security,
    Tooling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Clause {
    ordinal: usize,
    role: Role,
    tasks: BTreeSet<Task>,
    domains: BTreeSet<Domain>,
    means_of: Option<usize>,
}

impl Clause {
    #[must_use]
    const fn unresolved(ordinal: usize) -> Self {
        Self {
            ordinal,
            role: Role::Unresolved,
            tasks: BTreeSet::new(),
            domains: BTreeSet::new(),
            means_of: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Intent {
    // Independent request-only representation. Missing intent retains legacy cue semantics;
    // unknown versions reject dispatch, and persisted categorization labels are unchanged.
    version: u8,
    complete: bool,
    clauses: Vec<Clause>,
}

impl Intent {
    pub(super) fn valid(&self) -> bool {
        self.version == 1
            && self.clauses.len() <= MAX_CLAUSES
            && self.clauses.iter().enumerate().all(|(index, clause)| {
                clause.ordinal == index
                    && (clause.role == Role::Step) == clause.means_of.is_some()
                    && (clause.role != Role::Purpose || clause.tasks.len() == 1)
                    && clause.means_of.is_none_or(|target| {
                        clause.role == Role::Step
                            && target != index
                            && self
                                .clauses
                                .get(target)
                                .is_some_and(|goal| goal.role == Role::Purpose)
                    })
                    && (self.complete || clause.role == Role::Unresolved)
            })
    }
}

fn task(word: &str) -> Option<Task> {
    match word {
        "implement" | "add" | "build" => Some(Task::Implementation),
        "debug" | "fix" | "bug" => Some(Task::Debugging),
        "review" | "audit" => Some(Task::Review),
        "test" | "tests" | "testing" => Some(Task::Testing),
        "docs" | "documentation" | "document" => Some(Task::Documentation),
        "plan" | "design" => Some(Task::Planning),
        _ => None,
    }
}

fn is_action(word: &str) -> bool {
    task(word).is_some()
        || matches!(
            word,
            "implementing"
                | "adding"
                | "building"
                | "debugging"
                | "fixing"
                | "reviewing"
                | "auditing"
                | "testing"
                | "documenting"
                | "planning"
                | "designing"
                | "repairing"
        )
}

fn domain(word: &str) -> Option<Domain> {
    match word {
        "frontend" | "ui" | "tui" | "rendering" => Some(Domain::Frontend),
        "backend" | "server" | "api" => Some(Domain::Backend),
        "database" | "storage" | "persistence" => Some(Domain::Persistence),
        "security" | "auth" | "permissions" => Some(Domain::Security),
        "cli" | "tooling" | "cargo" | "plugin" => Some(Domain::Tooling),
        _ => None,
    }
}

/// Replace opaque tokens with fixed gaps before extracting legacy cues as well.
/// This temporary local projection is never serialized or sent to a provider.
#[must_use]
pub(super) fn lexical_projection(input: &str) -> String {
    let bounded: String = input.chars().take(8192).collect();
    let mut quote = None;
    bounded
        .split_whitespace()
        .take(512)
        .map(|raw| {
            let opaque = opaque_word(raw, &mut quote);
            let word = raw.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
            if opaque || !word.chars().all(|c| c.is_ascii_alphabetic() || c == '\'') {
                if raw.ends_with(['.', ';', '!', '?']) {
                    "gap.".to_owned()
                } else {
                    "gap".to_owned()
                }
            } else {
                raw.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn opaque_word(raw: &str, quote: &mut Option<char>) -> bool {
    let mut opaque = quote.is_some();
    for c in raw.chars() {
        if matches!(c, '"' | '`' | '\'') {
            if c == '\'' && !raw.starts_with('\'') && !raw.ends_with('\'') {
                continue;
            }
            opaque = true;
            if *quote == Some(c) {
                *quote = None;
            } else if quote.is_none() {
                *quote = Some(c);
            }
        }
    }
    opaque
}

/// Only explicit within-sentence `goal by means` / `means to goal` relationships.
/// Position alone never establishes purpose. Unknown syntax is unresolved.
#[must_use]
pub(super) fn extract(input: &str) -> Intent {
    let mut chars = input.chars();
    let bounded: String = chars.by_ref().take(8192).collect();
    let mut complete = chars.next().is_none();
    let mut words = bounded.split_whitespace();
    let mut clauses = Vec::new();
    let mut current = Clause::unresolved(0);
    let mut quote = None;
    let mut connector = None;
    let mut pending_action = false;
    let mut negated = false;
    let mut at_head = true;
    let mut action_head = false;
    for raw in words.by_ref().take(512) {
        if clauses.len() == MAX_CLAUSES {
            complete = false;
            break;
        }
        // Quotes and code are opaque locally too. No identifier substring matching.
        let opaque = opaque_word(raw, &mut quote);
        let word = raw
            .trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
            .to_ascii_lowercase();
        let safe = !opaque && word.chars().all(|c| c.is_ascii_alphabetic() || c == '\'');
        if at_head {
            action_head = safe && task(&word).is_some();
            at_head = false;
        }
        if pending_action && !(safe && is_action(&word)) {
            // An explicit connector without a supported action is not a means relation.
            connector = None;
        }
        pending_action = false;
        if safe
            && matches!(
                word.as_str(),
                "no" | "not" | "never" | "without" | "avoid" | "don't" | "cannot" | "can't"
            )
        {
            negated = true;
        }
        if safe
            && matches!(
                word.as_str(),
                "and" | "or" | "but" | "if" | "unless" | "rather"
            )
        {
            connector = None;
            action_head = false;
        }
        if safe
            && matches!(word.as_str(), "by" | "to")
            && action_head
            && current.tasks.len() == 1
            && !negated
            && connector.is_none()
        {
            connector = Some((word == "by", clauses.len()));
            pending_action = true;
            clauses.push(current);
            current = Clause::unresolved(clauses.len());
        } else if safe {
            current.tasks.extend(task(&word));
            current.domains.extend(domain(&word));
        }
        if negated {
            current.role = Role::Prohibition;
        }
        if raw.ends_with(['.', ';', '!', '?']) && quote.is_none() {
            finish(
                &mut clauses,
                &mut current,
                connector.take().filter(|_| !pending_action),
                negated,
            );
            pending_action = false;
            at_head = true;
            action_head = false;
            negated = false;
        }
    }
    complete &= words.next().is_none() && quote.is_none();
    if clauses.len() < MAX_CLAUSES {
        finish(
            &mut clauses,
            &mut current,
            connector.filter(|_| !pending_action),
            negated,
        );
    } else {
        complete = false;
    }
    if !complete {
        for clause in &mut clauses {
            clause.role = Role::Unresolved;
            clause.means_of = None;
        }
    }
    Intent {
        version: 1,
        complete,
        clauses,
    }
}

fn finish(
    clauses: &mut Vec<Clause>,
    current: &mut Clause,
    connector: Option<(bool, usize)>,
    negated: bool,
) {
    if let Some((by, previous)) = connector {
        // An opaque/unknown means clause may be represented, but not interpreted as
        // a known task. A goal must contain an actual fixed-vocabulary task signal.
        if !negated && (by || current.tasks.len() == 1) {
            if by {
                clauses[previous].role = Role::Purpose;
                current.role = Role::Step;
                current.means_of = Some(previous);
            } else {
                clauses[previous].role = Role::Step;
                clauses[previous].means_of = Some(current.ordinal);
                current.role = Role::Purpose;
            }
        }
    }
    if clauses.len() < MAX_CLAUSES {
        let ordinal = clauses.len() + 1;
        let next = Clause::unresolved(ordinal);
        clauses.push(std::mem::replace(current, next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_goal_and_means_work_in_both_directions() {
        for (text, roles) in [
            (
                "Document tooling by implementing an example.",
                [Role::Purpose, Role::Step],
            ),
            (
                "Implement tooling to document an example.",
                [Role::Step, Role::Purpose],
            ),
        ] {
            let facts = extract(text);
            assert!(facts.valid());
            assert_eq!(
                facts.clauses[..2]
                    .iter()
                    .map(|c| c.role)
                    .collect::<Vec<_>>(),
                roles
            );
        }
        let facts = extract("Review frontend by testing backend.");
        assert_eq!(facts.clauses[0].domains, BTreeSet::from([Domain::Frontend]));
        assert_eq!(facts.clauses[1].domains, BTreeSet::from([Domain::Backend]));
        assert_eq!(facts.clauses[1].means_of, Some(0));
    }

    #[test]
    fn independent_goals_and_position_do_not_establish_purpose() {
        for text in [
            "Document tooling. Implement backend.",
            "Implement backend and document tooling.",
            "Document tooling by implement backend and review frontend.",
            "Document tooling by tomorrow.",
            "A note says document tooling by implement backend.",
            "Review frontend before testing backend.",
        ] {
            assert!(
                extract(text)
                    .clauses
                    .iter()
                    .all(|c| c.role == Role::Unresolved)
            );
        }
    }

    #[test]
    fn prohibitions_and_opaque_cues_cannot_establish_purpose() {
        for text in [
            "Do not document tooling by implementing backend.",
            "Document tooling by not implementing backend.",
            "\"Document tooling by implementing backend\"",
            "`document tooling by implement backend`",
            "token=document /private/implement tooling_document",
        ] {
            assert!(
                extract(text)
                    .clauses
                    .iter()
                    .all(|c| c.role != Role::Purpose)
            );
        }
    }

    #[test]
    fn secret_substitutions_have_identical_sanitized_facts() {
        let left = extract("Document tooling by implementing token=SECRET /private/customer.");
        let right = extract("Document tooling by implementing token=OTHER /another/location.");
        assert_eq!(left, right);
        let encoded = serde_json::to_string(&left).unwrap();
        for secret in ["SECRET", "token", "private", "customer"] {
            assert!(!encoded.contains(secret));
        }
    }

    #[test]
    fn bounds_and_partial_quotes_clear_role_claims() {
        for text in [
            format!("Document tooling by implement {}", "x".repeat(8192)),
            format!("Document tooling by implement {}", "x ".repeat(600)),
            "Document tooling by implement. ".repeat(40),
            "Document tooling by implement \"unterminated".to_owned(),
        ] {
            let facts = extract(&text);
            assert!(!facts.complete);
            assert!(facts.valid());
            assert!(facts.clauses.len() <= MAX_CLAUSES);
            assert!(
                facts
                    .clauses
                    .iter()
                    .all(|c| c.role == Role::Unresolved && c.means_of.is_none())
            );
        }
    }

    #[test]
    fn unknown_versions_and_enum_values_fail_closed() {
        let mut value =
            serde_json::to_value(extract("Document tooling by implement backend.")).unwrap();
        value["version"] = serde_json::json!(2);
        assert!(
            !serde_json::from_value::<Intent>(value.clone())
                .unwrap()
                .valid()
        );
        value["version"] = serde_json::json!(false);
        assert!(serde_json::from_value::<Intent>(value.clone()).is_err());
        value["version"] = serde_json::json!(1);
        value["clauses"][0]["role"] = serde_json::json!("SECRET");
        assert!(serde_json::from_value::<Intent>(value).is_err());
    }

    #[test]
    fn original_mixed_request_keeps_goal_and_unresolved_later_instruction() {
        let facts = extract(
            "Document tooling path normalization by repairing its executable example. Repair repair.py for the local service contract. Implement one pure return expression.",
        );
        assert_eq!(facts.clauses[0].role, Role::Purpose);
        assert_eq!(
            facts.clauses[0].tasks,
            BTreeSet::from([Task::Documentation])
        );
        assert_eq!(facts.clauses[1].role, Role::Step);
        assert!(
            facts
                .clauses
                .iter()
                .any(|c| c.role == Role::Unresolved && c.tasks.contains(&Task::Implementation))
        );
    }
}
