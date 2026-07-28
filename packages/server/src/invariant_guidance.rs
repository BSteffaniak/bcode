//! Repository invariant catalog parsing, focused selection validation, and reminder formatting.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// One invariant parsed from the repository catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantEntry {
    pub index: usize,
    pub section: String,
    pub title: Option<String>,
    pub text: String,
}

/// Parsed repository invariant catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantCatalog {
    pub raw: String,
    pub digest: String,
    pub entries: Vec<InvariantEntry>,
}

/// Valid focused guidance prepared for a coding-model request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantGuidance {
    pub catalog_digest: String,
    pub task_summary: Option<String>,
    pub selected: Vec<InvariantEntry>,
    pub source_sequence: u64,
}

#[derive(Debug, Deserialize)]
struct SelectorOutput {
    #[serde(default)]
    task_summary: Option<String>,
    selected: Vec<usize>,
}

/// Parse the simple section-and-bullet invariant catalog.
pub fn parse_catalog(contents: &str) -> Option<InvariantCatalog> {
    let raw = contents.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let digest = hex_digest(raw.as_bytes());
    let mut section = String::new();
    let mut entries = Vec::new();
    let mut pending: Option<String> = None;

    for line in raw.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            flush_entry(&mut entries, &section, pending.take());
            section = heading.trim().to_string();
            continue;
        }
        if let Some(bullet) = line.strip_prefix("* ") {
            flush_entry(
                &mut entries,
                &section,
                pending.replace(bullet.trim().to_string()),
            );
            continue;
        }
        if let Some(current) = pending.as_mut() {
            let continuation = line.trim();
            if !continuation.is_empty() {
                current.push(' ');
                current.push_str(continuation);
            }
        }
    }
    flush_entry(&mut entries, &section, pending);
    (!entries.is_empty()).then_some(InvariantCatalog {
        raw,
        digest,
        entries,
    })
}

fn flush_entry(entries: &mut Vec<InvariantEntry>, section: &str, text: Option<String>) {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return;
    };
    let (title, body) = split_title(&text);
    entries.push(InvariantEntry {
        index: entries.len() + 1,
        section: section.to_string(),
        title,
        text: body,
    });
}

fn split_title(text: &str) -> (Option<String>, String) {
    let Some(rest) = text.strip_prefix("**") else {
        return (None, text.to_string());
    };
    let Some(end) = rest.find("**") else {
        return (None, text.to_string());
    };
    let title = rest[..end].trim().trim_end_matches('.').to_string();
    let body = rest[end + 2..].trim().to_string();
    let full = if body.is_empty() {
        title.clone()
    } else {
        format!("**{title}.** {body}")
    };
    (Some(title), full)
}

/// Format the selector-only catalog with request-local numeric references.
pub fn selector_catalog(catalog: &InvariantCatalog) -> String {
    catalog
        .entries
        .iter()
        .map(|entry| format!("{}. [{}] {}", entry.index, entry.section, entry.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decode selector JSON and resolve only exact catalog references.
pub fn guidance_from_selector(
    catalog: &InvariantCatalog,
    response: &str,
    max_selected: usize,
    source_sequence: u64,
    include_task_summary: bool,
) -> Option<InvariantGuidance> {
    let json = extract_json(response);
    let output: SelectorOutput = serde_json::from_str(json).ok()?;
    let mut seen = BTreeSet::new();
    let selected = output
        .selected
        .into_iter()
        .filter(|index| seen.insert(*index))
        .filter_map(|index| catalog.entries.get(index.checked_sub(1)?).cloned())
        .take(max_selected)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return None;
    }
    Some(InvariantGuidance {
        catalog_digest: catalog.digest.clone(),
        task_summary: include_task_summary
            .then(|| output.task_summary.unwrap_or_default().trim().to_string())
            .filter(|summary| !summary.is_empty()),
        selected,
        source_sequence,
    })
}

fn extract_json(response: &str) -> &str {
    let trimmed = response.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    trimmed.strip_suffix("```").unwrap_or(trimmed).trim()
}

/// Select a bounded local fallback using task/title/section token overlap.
pub fn deterministic_guidance(
    catalog: &InvariantCatalog,
    task: &str,
    max_selected: usize,
    source_sequence: u64,
) -> Option<InvariantGuidance> {
    let task_tokens = tokens(task);
    let mut ranked = catalog
        .entries
        .iter()
        .map(|entry| {
            let candidate = format!(
                "{} {} {}",
                entry.section,
                entry.title.as_deref().unwrap_or_default(),
                entry.text
            );
            let score = tokens(&candidate).intersection(&task_tokens).count();
            (score, entry.index, entry.clone())
        })
        .filter(|(score, _, _)| *score > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    let selected = ranked
        .into_iter()
        .take(max_selected)
        .map(|(_, _, entry)| entry)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return None;
    }
    Some(InvariantGuidance {
        catalog_digest: catalog.digest.clone(),
        task_summary: None,
        selected,
        source_sequence,
    })
}

fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 4)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Format the compact reminder supplied to the primary coding model.
pub fn format_reminder(guidance: &InvariantGuidance) -> String {
    let mut reminder = String::from(
        "Relevant repository invariants\n\nTreat the following focused invariant selection as mandatory for the current work.",
    );
    if let Some(summary) = &guidance.task_summary {
        reminder.push_str("\n\nCurrent task: ");
        reminder.push_str(summary);
    }
    reminder.push_str("\n\n");
    reminder.push_str(
        &guidance
            .selected
            .iter()
            .map(|entry| format!("* {}", entry.text))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    reminder
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").expect("writing to string");
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = "# Invariants\n\n## Rendering\n\n* **Shared rendering remains neutral.** No terminal types.\n* Renderers preserve semantics.\n\n## Sessions\n\n* **Reads are bounded.** Normal reads do not repair.";

    #[test]
    fn parses_sections_and_simple_bullets() {
        let catalog = parse_catalog(CATALOG).expect("catalog");
        assert_eq!(catalog.entries.len(), 3);
        assert_eq!(catalog.entries[0].section, "Rendering");
        assert_eq!(
            catalog.entries[0].title.as_deref(),
            Some("Shared rendering remains neutral")
        );
        assert!(catalog.entries[0].text.contains("No terminal types"));
    }

    #[test]
    fn selector_references_are_exact_bounded_and_deduplicated() {
        let catalog = parse_catalog(CATALOG).expect("catalog");
        let guidance = guidance_from_selector(
            &catalog,
            r#"{"task_summary":"render UI","selected":[1,1,99,3]}"#,
            2,
            7,
            true,
        )
        .expect("guidance");
        assert_eq!(guidance.selected.len(), 2);
        assert_eq!(guidance.selected[0].index, 1);
        assert_eq!(guidance.selected[1].index, 3);
        assert_eq!(guidance.task_summary.as_deref(), Some("render UI"));
    }

    #[test]
    fn reminder_contains_only_selected_invariants() {
        let catalog = parse_catalog(CATALOG).expect("catalog");
        let guidance = guidance_from_selector(
            &catalog,
            r#"{"task_summary":"render UI","selected":[1]}"#,
            6,
            1,
            true,
        )
        .expect("guidance");
        let reminder = format_reminder(&guidance);
        assert!(reminder.contains("Shared rendering remains neutral"));
        assert!(!reminder.contains("Reads are bounded"));
        assert!(!reminder.contains("Renderers preserve semantics"));
    }

    #[test]
    fn deterministic_selection_is_bounded() {
        let catalog = parse_catalog(CATALOG).expect("catalog");
        let guidance =
            deterministic_guidance(&catalog, "terminal renderer", 1, 1).expect("guidance");
        assert_eq!(guidance.selected.len(), 1);
        assert_eq!(guidance.selected[0].index, 1);
    }
}
