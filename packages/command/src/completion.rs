//! Portable completion matching, independent of discovery source and renderer.

/// Completion matching policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompletionMatchMode {
    /// Allow scored subsequences after contiguous matches.
    #[default]
    Fuzzy,
    /// Match only leading text.
    Prefix,
    /// Match contiguous text.
    Substring,
}

/// Completion ordering policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSortOrder {
    /// Order by match quality.
    Relevance,
    /// Order by normalized command name.
    Alphabetical,
}

/// Comparable match quality, best first. Command matches precede description matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompletionMatchRank {
    description_only: bool,
    tier: u8,
    score: usize,
}

/// Normalize completion text without imposing renderer-specific geometry.
#[must_use]
pub fn normalize_completion(value: &str) -> String {
    value
        .trim_start_matches('/')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rank a completion by name first, falling back to its description.
#[allow(
    clippy::must_use_candidate,
    reason = "Option is already must-use; repository policy forbids redundant function attributes"
)]
pub fn completion_match_rank(
    command: &str,
    description: &str,
    query: &str,
    mode: CompletionMatchMode,
) -> Option<CompletionMatchRank> {
    let query = normalize_completion(query);
    text_rank(&normalize_completion(command), &query, mode)
        .map(|(tier, score)| CompletionMatchRank {
            description_only: false,
            tier,
            score,
        })
        .or_else(|| {
            text_rank(&normalize_completion(description), &query, mode).map(|(tier, score)| {
                CompletionMatchRank {
                    description_only: true,
                    tier,
                    score,
                }
            })
        })
}

fn text_rank(value: &str, query: &str, mode: CompletionMatchMode) -> Option<(u8, usize)> {
    if value == query {
        return Some((0, 0));
    }
    if value.starts_with(query) {
        return Some((
            1,
            value.chars().count().saturating_sub(query.chars().count()),
        ));
    }
    if mode == CompletionMatchMode::Prefix {
        return None;
    }
    if let Some((offset, boundary)) = value
        .match_indices(query)
        .map(|(offset, _)| {
            (
                offset,
                value[..offset]
                    .chars()
                    .next_back()
                    .is_none_or(|ch| !ch.is_alphanumeric()),
            )
        })
        .min_by_key(|(offset, boundary)| (!*boundary, *offset))
    {
        return Some((if boundary { 2 } else { 3 }, offset));
    }
    if mode != CompletionMatchMode::Fuzzy {
        return None;
    }
    // Keep the cheapest partial subsequence, rather than committing to the first
    // occurrence: a later start can avoid a much larger gap. Work is O(name * query)
    // with O(query) memory, and no renderer or discovery-source inputs.
    let query = query.chars().collect::<Vec<_>>();
    let mut partial: Vec<Option<usize>> = vec![None; query.len()];
    let mut best = None;
    for (index, ch) in value.chars().enumerate() {
        for matched in (0..query.len()).rev() {
            let candidate = if ch == query[matched] {
                if matched == 0 {
                    Some(index)
                } else {
                    partial[matched - 1]
                }
            } else {
                None
            };
            if matched + 1 == query.len()
                && let Some(candidate) = candidate
            {
                best = Some(best.map_or(candidate, |score: usize| score.min(candidate)));
            }
            let skipped = partial[matched].map(|score| score + 2);
            partial[matched] = match (skipped, candidate) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            };
        }
    }
    best.map(|score| (4, score))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_always_beat_description_only_matches() {
        let name =
            completion_match_rank("/workflow.resume", "", "resume", CompletionMatchMode::Fuzzy)
                .unwrap();
        let description =
            completion_match_rank("/other", "resume", "resume", CompletionMatchMode::Fuzzy)
                .unwrap();
        assert!(name < description);
    }

    #[test]
    fn resume_prefix_precedes_nested_commands() {
        let prefix = completion_match_rank(
            "/resume-progress-doc",
            "",
            "/resume",
            CompletionMatchMode::Fuzzy,
        )
        .unwrap();
        for command in [
            "/ralph resume",
            "/goal.resume",
            "/loop.resume",
            "/workflow.resume",
        ] {
            assert!(
                prefix
                    < completion_match_rank(command, "", "resume", CompletionMatchMode::Fuzzy)
                        .unwrap()
            );
        }
    }

    #[test]
    fn fuzzy_scoring_uses_best_subsequence_not_first_occurrence() {
        let best = completion_match_rank("r----------r-es", "", "res", CompletionMatchMode::Fuzzy)
            .unwrap();
        let alternate =
            completion_match_rank("r---------e--s", "", "res", CompletionMatchMode::Fuzzy).unwrap();
        assert!(best < alternate);
        assert_eq!(best.score, 13);
    }

    #[test]
    fn tiers_and_modes_are_distinct() {
        let rank =
            |name| completion_match_rank(name, "", "res", CompletionMatchMode::Fuzzy).unwrap();
        assert!(rank("res") < rank("resume"));
        assert!(rank("resume") < rank("goal.res"));
        assert!(rank("goal.res") < rank("fores"));
        assert!(rank("fores") < rank("r-e-s"));
        assert!(
            completion_match_rank("goal.res", "", "res", CompletionMatchMode::Prefix).is_none()
        );
        assert!(
            completion_match_rank("r-e-s", "", "res", CompletionMatchMode::Substring).is_none()
        );
        assert!(rank("r-e-s") < rank("r---e---s"));
    }
}
