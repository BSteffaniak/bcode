//! Literal replacements resolved against a single immutable UTF-8 snapshot.

/// Construct replacement output without touching the filesystem.
///
/// Each search must match once; ranges from different edits must not overlap.
/// Input indices are retained in diagnostics, regardless of source order.
/// `allow_overlapping_matches` retains legacy single-edit occurrence counting;
/// batch callers must pass false so overlapping occurrences are ambiguous too.
pub fn replace_snapshot(
    source: &str,
    edits: &[(&str, &str)],
    max_output_bytes: usize,
    allow_overlapping_matches: bool,
) -> Result<(String, usize), String> {
    replace_snapshot_cancellable(
        source,
        edits,
        max_output_bytes,
        allow_overlapping_matches,
        &|| false,
    )
}

/// Construct literal replacements with cancellation between searches and copies.
/// Each individual string search remains bounded by the caller's source budget.
pub fn replace_snapshot_cancellable(
    source: &str,
    edits: &[(&str, &str)],
    max_output_bytes: usize,
    allow_overlapping_matches: bool,
    cancelled: &impl Fn() -> bool,
) -> Result<(String, usize), String> {
    if cancelled() {
        return Err("replacement cancelled before mutation".to_owned());
    }
    if edits.is_empty() {
        return Err("edits must not be empty".to_owned());
    }
    let mut ranges = Vec::with_capacity(edits.len());
    let mut output_bytes = source.len();
    for (index, &(old, new)) in edits.iter().enumerate() {
        if cancelled() {
            return Err(format!(
                "edit {}: replacement cancelled before mutation",
                index + 1
            ));
        }
        if old.is_empty() {
            return Err(format!("edit {}: old_text must not be empty", index + 1));
        }
        let Some(start) = source.find(old) else {
            return Err(format!("edit {}: old_text has no match", index + 1));
        };
        let next_offset = if allow_overlapping_matches {
            start + old.len()
        } else {
            // Advance one Unicode scalar, not the entire match: a second
            // occurrence can begin inside the first, including multibyte text.
            start + old.chars().next().expect("nonempty search").len_utf8()
        };
        if source[next_offset..].contains(old) {
            return Err(format!("edit {}: old_text is ambiguous", index + 1));
        }
        output_bytes = output_bytes
            .checked_sub(old.len())
            .and_then(|bytes| bytes.checked_add(new.len()))
            .ok_or_else(|| format!("edit {}: output size overflow", index + 1))?;
        ranges.push((start, start + old.len(), index, new));
    }
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "edits {} and {} overlap",
                pair[0].2 + 1,
                pair[1].2 + 1
            ));
        }
    }
    if output_bytes > max_output_bytes {
        return Err(format!(
            "replacement output exceeds {max_output_bytes} bytes"
        ));
    }
    let first_offset = ranges[0].0;
    let mut output = String::new();
    output
        .try_reserve_exact(output_bytes)
        .map_err(|_| "cannot allocate bounded replacement output".to_owned())?;
    if output.capacity() > max_output_bytes {
        return Err("replacement allocation exceeds byte budget".to_owned());
    }
    let mut cursor = 0;
    for (start, end, _, replacement) in ranges {
        if cancelled() {
            return Err("replacement cancelled before mutation".to_owned());
        }
        output.push_str(&source[cursor..start]);
        output.push_str(replacement);
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    if cancelled() {
        return Err("replacement cancelled before mutation".to_owned());
    }
    Ok((output, first_offset))
}

#[cfg(test)]
mod tests {
    fn replace_snapshot(
        source: &str,
        edits: &[(&str, &str)],
        max_output_bytes: usize,
    ) -> Result<(String, usize), String> {
        super::replace_snapshot(source, edits, max_output_bytes, false)
    }

    #[test]
    fn output_budget_applies_to_final_snapshot_not_intermediate_edit_order() {
        let edits = [("a", "expanded"), ("long suffix", "")];
        let reversed = [edits[1], edits[0]];
        for edits in [&edits, &reversed] {
            let (output, _) = replace_snapshot("a long suffix", edits, 9).unwrap();
            assert_eq!(output, "expanded ");
            assert!(output.capacity() <= 9);
            assert!(replace_snapshot("a long suffix", edits, 8).is_err());
        }
        assert_eq!(replace_snapshot("a", &[("a", "")], 0).unwrap().0, "");
    }

    #[test]
    fn cancellation_interrupts_matching_and_output_construction() {
        for stop_at in [1, 3, 4, 5, 6] {
            let polls = std::cell::Cell::new(0);
            let result = super::replace_snapshot_cancellable(
                "first second",
                &[("first", "1"), ("second", "2")],
                100,
                false,
                &|| {
                    polls.set(polls.get() + 1);
                    polls.get() == stop_at
                },
            );
            assert!(result.unwrap_err().contains("cancelled"));
            assert_eq!(polls.get(), stop_at);
        }
        assert_eq!(
            super::replace_snapshot_cancellable(
                "first second",
                &[("first", "1"), ("second", "2")],
                100,
                false,
                &|| false,
            )
            .unwrap()
            .0,
            "1 2"
        );
    }

    #[test]
    fn overlapping_occurrences_are_ambiguous_without_changing_legacy_matching() {
        for (source, search, expected) in [("aaa", "aa", "xa"), ("🙂🙂🙂", "🙂🙂", "x🙂")]
        {
            assert!(
                replace_snapshot(source, &[(search, "x")], 100)
                    .unwrap_err()
                    .contains("edit 1: old_text is ambiguous")
            );
            assert_eq!(
                super::replace_snapshot(source, &[(search, "x")], 100, true)
                    .unwrap()
                    .0,
                expected
            );
        }
    }

    #[test]
    fn original_snapshot_not_request_order_controls_matching() {
        let source = "alpha beta";
        assert_eq!(
            replace_snapshot(source, &[("beta", "gamma"), ("alpha", "beta")], 100)
                .unwrap()
                .0,
            "beta gamma"
        );
        assert!(replace_snapshot(source, &[("alpha", "new"), ("new", "later")], 100).is_err());
    }

    #[test]
    fn rejects_invalid_and_overlapping_edits() {
        for edits in [
            vec![],
            vec![("", "x")],
            vec![("missing", "x")],
            vec![("a", "x")],
            vec![("abc", "x"), ("bc", "y")],
        ] {
            assert!(replace_snapshot("abc abcdef", &edits, 100).is_err());
        }
        assert!(
            replace_snapshot("abcdef", &[("abc", "x"), ("bc", "y")], 100)
                .unwrap_err()
                .contains("overlap")
        );
    }

    #[test]
    fn preserves_untouched_utf8_bom_and_mixed_newlines() {
        let source = "\u{feff}甲\r\nold\n🙂\r\nend";
        assert_eq!(
            replace_snapshot(source, &[("old", "new")], 100)
                .unwrap()
                .0
                .as_bytes(),
            "\u{feff}甲\r\nnew\n🙂\r\nend".as_bytes()
        );
    }

    #[test]
    fn bounds_output_before_allocation_and_accepts_adjacent_ranges() {
        assert!(replace_snapshot("ab", &[("a", "long")], 2).is_err());
        assert_eq!(
            replace_snapshot("ab", &[("b", ""), ("a", "z")], 1).unwrap(),
            ("z".to_owned(), 0)
        );
    }
}
