//! Portable whole-path matching. No terminal geometry or renderer dependencies.
use super::SearchPathMatchRange;

pub(super) struct Match {
    pub score: usize,
    pub ranges: Vec<SearchPathMatchRange>,
}

pub(super) fn mask(text: &str) -> u64 {
    text.bytes()
        .fold(0, |bits, byte| bits | (1_u64 << (byte & 63)))
}

pub(super) struct Query {
    lower: String,
    chars: Vec<char>,
    mask: u64,
}
impl Query {
    pub fn new(query: &str) -> Self {
        let lower = query.to_lowercase();
        Self {
            chars: lower.chars().collect(),
            mask: mask(&lower),
            lower,
        }
    }
    /// Linear whole-path subsequence scoring without allocation or clock reads.
    pub fn score(&self, lower: &str, path_mask: u64) -> Option<usize> {
        if self.lower.is_empty() {
            return Some(0);
        }
        if self.mask & path_mask != self.mask {
            return None;
        }
        let basename = lower.rfind('/').map_or(0, |offset| offset + 1);
        let name = &lower[basename..];
        let category = if lower == self.lower || name == self.lower {
            0
        } else if name.starts_with(&self.lower) {
            1
        } else if lower.contains(&self.lower) {
            2
        } else {
            3
        };
        if category < 3 {
            let start = if name.starts_with(&self.lower) {
                basename
            } else {
                lower.find(&self.lower)?
            };
            return Some(category * 1_000_000 + start);
        }
        let mut needle = 0;
        let mut first = 0;
        let mut last = 0;
        let mut boundaries = 0;
        let mut previous = None;
        for (index, character) in lower.chars().enumerate() {
            if character == self.chars[needle] {
                if needle == 0 {
                    first = index;
                }
                last = index;
                boundaries +=
                    usize::from(previous.is_none_or(|c| matches!(c, '/' | '_' | '-' | '.' | ' ')));
                needle += 1;
                if needle == self.chars.len() {
                    break;
                }
            }
            previous = Some(character);
        }
        if needle != self.chars.len() {
            return None;
        }
        let gaps = last - first + 1 - needle;
        Some(3_000_000 + (gaps * 32 + (needle - boundaries) * 8 + first).min(999_999))
    }
    pub fn ranges(&self, path: &str) -> Vec<SearchPathMatchRange> {
        let mut needle = 0;
        let mut ranges: Vec<SearchPathMatchRange> = Vec::new();
        if self.chars.is_empty() {
            return ranges;
        }
        for (start, character) in path.char_indices() {
            for folded in character.to_lowercase() {
                if folded != self.chars[needle] {
                    continue;
                }
                let end = start + character.len_utf8();
                if let Some(last) = ranges.last_mut().filter(|range| range.end >= start) {
                    last.end = last.end.max(end);
                } else {
                    ranges.push(SearchPathMatchRange { start, end });
                }
                needle += 1;
                if needle == self.chars.len() {
                    return ranges;
                }
            }
        }
        ranges
    }
}

pub(super) fn find(
    path: &str,
    query: &str,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Option<Match>, &'static str> {
    tick()?;
    let query = Query::new(query);
    let lower = path.to_lowercase();
    let matched = query.score(&lower, mask(&lower)).map(|score| Match {
        score,
        ranges: query.ranges(path),
    });
    tick()?;
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn score(path: &str, query: &str) -> usize {
        find(path, query, &mut || Ok(())).unwrap().unwrap().score
    }
    #[test]
    fn whole_path_and_ranking() {
        assert!(
            find("src/how-to-my-life.txt", "shtml", &mut || Ok(()))
                .unwrap()
                .is_some()
        );
        assert!(score("src/foo", "foo") < score("src/foobar", "foo"));
        assert!(score("src/foobar", "foo") < score("src/xfoobar", "foo"));
        assert!(score("src/xfoobar", "foo") < score("src/f-o-o", "foo"));
        assert!(score("src/foo-bar", "fb") < score("src/fxxxxb", "fb"));
        assert!(score("src/fb", "fb") < score("src/far-away-b", "fb"));
    }
    #[test]
    fn original_unicode_byte_ranges() {
        let path = "src/İ中😀.rs";
        let matched = find(path, "i\u{307}中😀", &mut || Ok(())).unwrap().unwrap();
        assert_eq!(
            &path[matched.ranges[0].start..matched.ranges[0].end],
            "İ中😀"
        );
        assert!(find(path, "absent", &mut || Ok(())).unwrap().is_none());
        assert!(find(path, "中", &mut || Err("cancelled")).is_err());
    }
}
