//! Bounded formatter memoization; literal drafts never enter this cache.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const MAX_ENTRIES: usize = 16;
const MAX_BYTES: usize = 512 * 1024;
// All formatter options are fixed here. Exact source is the remaining cache key.
const OPTIONS: (u8, u8, u8) = (1, 1, 2); // Bash, spaces, two-cell indent.
type CacheEntry = ((u8, u8, u8), String, String);

pub(super) fn display(source: &str) -> String {
    static CACHE: OnceLock<Mutex<VecDeque<CacheEntry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    if let Ok(entries) = cache.lock()
        && let Some((_, _, display)) = entries
            .iter()
            .find(|(options, key, _)| *options == OPTIONS && key == source)
    {
        return display.clone();
    }
    let display = format(source);
    let bytes = source.len().saturating_add(display.len());
    if bytes <= MAX_BYTES
        && let Ok(mut entries) = cache.lock()
    {
        // Concurrent misses must not multiply entries for the same source/options.
        entries.retain(|(options, key, _)| *options != OPTIONS || key != source);
        let mut retained = entries
            .iter()
            .map(|(_, key, value)| key.len() + value.len())
            .sum::<usize>();
        while entries.len() >= MAX_ENTRIES || retained.saturating_add(bytes) > MAX_BYTES {
            let Some((_, key, value)) = entries.pop_front() else {
                break;
            };
            retained = retained.saturating_sub(key.len() + value.len());
        }
        entries.push_back((OPTIONS, source.to_owned(), display.clone()));
    }
    display
}

fn format(command: &str) -> String {
    use shuck_formatter::{
        FormattedSource, IndentStyle, ShellDialect, ShellFormatOptions, format_source,
    };
    let options = ShellFormatOptions::default()
        .with_dialect(ShellDialect::Bash)
        .with_indent_style(IndentStyle::Space)
        .with_indent_width(2);
    match format_source(command, None, &options) {
        Ok(FormattedSource::Formatted(formatted)) => {
            formatted.trim_end_matches(['\r', '\n']).to_owned()
        }
        Ok(FormattedSource::Unchanged) | Err(_) => command.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_source_and_fallback_survive_cache_churn() {
        let invalid = "if unfinished '  quoted";
        assert_eq!(display(invalid), invalid);
        let source = "if true;then echo 'two  spaces';fi";
        let expected = display(source);
        for index in 0..MAX_ENTRIES * 2 {
            assert_eq!(
                display(&format!("echo '{index}  value'")),
                format!("echo '{index}  value'")
            );
        }
        assert_eq!(display(source), expected);
        assert_eq!(display(invalid), invalid);
        assert_ne!(display("echo 'a  b'"), display("echo 'a b'"));
    }
}
