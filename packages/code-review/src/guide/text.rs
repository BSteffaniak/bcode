//! Line normalization, hashing, excerpts, and matching shared by citation checks and freshness.

use bcode_code_review_models::{MAX_GUIDE_EXCERPT_BYTES, ReviewLineInterval};
use sha2::{Digest as _, Sha256};

/// A line as compared: trailing whitespace (and a carriage return) removed.
pub fn normalized(line: &str) -> &str {
    line.trim_end()
}

/// A line as compared ignoring indentation.
pub fn loose(line: &str) -> &str {
    line.trim()
}

/// SHA-256 (hex) of `lines`, normalized and joined with newlines.
pub fn quote_hash<S: AsRef<str>>(lines: &[S]) -> String {
    let mut hasher = Sha256::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            hasher.update(b"\n");
        }
        hasher.update(normalized(line.as_ref()).as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The first of `lines`, joined with newlines, within [`MAX_GUIDE_EXCERPT_BYTES`]. A first line
/// longer than the bound is cut at a character boundary.
pub fn excerpt<S: AsRef<str>>(lines: &[S]) -> String {
    let mut out = String::new();
    for line in lines {
        let line = line.as_ref();
        let needed = line.len() + usize::from(!out.is_empty());
        if out.len() + needed > MAX_GUIDE_EXCERPT_BYTES {
            if out.is_empty() {
                let mut end = MAX_GUIDE_EXCERPT_BYTES;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                out.push_str(&line[..end]);
            }
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// The lines of `lines` in `interval` (1-based, inclusive); `None` when out of bounds.
pub fn slice<S>(lines: &[S], interval: ReviewLineInterval) -> Option<&[S]> {
    let start = usize::try_from(interval.start).ok()?.checked_sub(1)?;
    let end = usize::try_from(interval.end).ok()?;
    (start < end && end <= lines.len()).then(|| &lines[start..end])
}

/// The interval of `count` lines starting at zero-based `index`.
pub fn interval_at(index: usize, count: usize) -> ReviewLineInterval {
    let start = u32::try_from(index + 1).unwrap_or(u32::MAX);
    let end = u32::try_from(index + count).unwrap_or(u32::MAX);
    ReviewLineInterval { start, end }
}

/// Zero-based positions where `needle` occurs in `haystack` as consecutive lines, compared with
/// `same`. At most `limit` positions are returned.
pub fn find_runs<H, N>(
    haystack: &[H],
    needle: &[N],
    same: impl Fn(&str, &str) -> bool,
    limit: usize,
) -> Vec<usize>
where
    H: AsRef<str>,
    N: AsRef<str>,
{
    let mut found = Vec::new();
    if needle.is_empty() || needle.len() > haystack.len() {
        return found;
    }
    let first = needle[0].as_ref();
    for start in 0..=haystack.len() - needle.len() {
        if !same(haystack[start].as_ref(), first) {
            continue;
        }
        let matches = needle
            .iter()
            .zip(&haystack[start..])
            .all(|(want, have)| same(have.as_ref(), want.as_ref()));
        if matches {
            found.push(start);
            if found.len() >= limit {
                break;
            }
        }
    }
    found
}

/// Exact comparison of normalized lines.
pub fn same_exact(have: &str, want: &str) -> bool {
    normalized(have) == normalized(want)
}

/// Indentation-insensitive comparison of lines.
pub fn same_loose(have: &str, want: &str) -> bool {
    loose(have) == loose(want)
}

/// Similarity of two lines in `0.0..=1.0`: the Dice coefficient of their character bigrams,
/// ignoring indentation and trailing whitespace.
pub fn line_similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (loose(a), loose(b));
    if a == b {
        return 1.0;
    }
    let bigrams = |text: &str| -> Vec<(char, char)> {
        let chars: Vec<char> = text.chars().collect();
        let mut pairs: Vec<(char, char)> =
            chars.windows(2).map(|pair| (pair[0], pair[1])).collect();
        pairs.sort_unstable();
        pairs
    };
    let (left, right) = (bigrams(a), bigrams(b));
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut shared) = (0, 0, 0_usize);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let score = (2 * shared) as f64 / (left.len() + right.len()) as f64;
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_ignore_trailing_whitespace_and_carriage_returns() {
        assert_eq!(quote_hash(&["a  ", "b\r"]), quote_hash(&["a", "b"]));
        assert_ne!(quote_hash(&["a", "b"]), quote_hash(&["a", " b"]));
    }

    #[test]
    fn excerpts_stay_within_their_bound_on_line_and_character_boundaries() {
        let long = "é".repeat(MAX_GUIDE_EXCERPT_BYTES);
        let cut = excerpt(&[long.as_str(), "next"]);
        assert!(cut.len() <= MAX_GUIDE_EXCERPT_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));
        let lines = vec!["x".repeat(1500), "y".repeat(1500)];
        assert_eq!(excerpt(&lines), lines[0]);
    }

    #[test]
    fn runs_are_found_by_consecutive_lines() {
        let haystack = ["a", "b", "c", "a", "b"];
        assert_eq!(find_runs(&haystack, &["a", "b"], same_exact, 10), [0, 3]);
        assert_eq!(find_runs(&haystack, &["b", "c"], same_exact, 10), [1]);
        assert_eq!(
            find_runs(&haystack, &["c", "b"], same_exact, 10),
            [] as [usize; 0]
        );
        assert_eq!(find_runs(&["  a"], &["a"], same_loose, 10), [0]);
    }

    #[test]
    fn similar_lines_score_high_and_different_lines_low() {
        assert!((line_similarity("let x = 1;", "  let x = 1;") - 1.0).abs() < f64::EPSILON);
        assert!(line_similarity("let budget = 10;", "let budget = 12;") > 0.8);
        assert!(line_similarity("let budget = 10;", "fn main() {}") < 0.3);
    }
}
