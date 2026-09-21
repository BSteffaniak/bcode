//! Bounded, read-only projection of goal working-document checklists.

use std::fmt::Write as _;
use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

const MAX_DOCUMENT_BYTES: usize = 65_536;
const MAX_TASKS: usize = 1_024;

/// One actual Markdown task item, in source order.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub checked: bool,
    /// Plain semantic label, excluding nested child items.
    pub label: String,
    pub depth: usize,
    pub source: Range<usize>,
}

/// Tasks grouped by heading, retaining phase subheadings within their phase.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    pub tasks: Vec<Task>,
}

/// Complete counts are available only when the entire document was inspected.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checklist {
    pub sections: Vec<Section>,
    pub truncated: bool,
}

impl Checklist {
    pub(crate) fn parse(text: &str) -> Self {
        let mut result = Self {
            sections: vec![Section {
                title: "Other checklist items".into(),
                tasks: Vec::new(),
            }],
            truncated: text.len() > MAX_DOCUMENT_BYTES,
        };
        // Never parse a chopped document as if it had a complete Markdown structure.
        if result.truncated {
            return result;
        }
        let mut heading = None;
        let mut items: Vec<Option<(usize, usize)>> = Vec::new();
        let mut phase_level = None;
        let mut heading_level = 0;
        let mut count = 0;
        for (event, source) in Parser::new_ext(text, Options::ENABLE_TASKLISTS).into_offset_iter() {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    heading_level = level as u8;
                    heading = Some(String::new());
                }
                Event::Text(value) | Event::Code(value) => {
                    if let Some(title) = &mut heading {
                        title.push_str(&value);
                    } else if let Some(Some((section, task))) = items.last() {
                        result.sections[*section].tasks[*task]
                            .label
                            .push_str(&value);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(Some((section, task))) = items.last() {
                        result.sections[*section].tasks[*task].label.push(' ');
                    }
                }
                Event::End(TagEnd::Heading(_)) => {
                    if let Some(title) = heading.take() {
                        let is_phase = title
                            .split_whitespace()
                            .next()
                            .is_some_and(|word| word.eq_ignore_ascii_case("phase"));
                        if is_phase || phase_level.is_none_or(|level| heading_level <= level) {
                            phase_level = is_phase.then_some(heading_level);
                            result.sections.push(Section {
                                title,
                                tasks: Vec::new(),
                            });
                        }
                    }
                }
                Event::Start(Tag::Item) => items.push(None),
                Event::End(TagEnd::Item) => {
                    if let Some(Some((section, task))) = items.pop() {
                        let task = &mut result.sections[section].tasks[task];
                        task.source.end = source.end;
                        task.label = task.label.trim().to_owned();
                    }
                }
                Event::TaskListMarker(checked) => {
                    if count == MAX_TASKS {
                        result.truncated = true;
                        break;
                    }
                    let section_index = result.sections.len() - 1;
                    let section = &mut result.sections[section_index];
                    if let Some(item) = items.last_mut() {
                        *item = Some((section_index, section.tasks.len()));
                    }
                    section.tasks.push(Task {
                        checked,
                        label: String::new(),
                        depth: items.len().saturating_sub(1),
                        source,
                    });
                    count += 1;
                }
                _ => {}
            }
        }
        result
    }

    pub(crate) fn summary(&self) -> String {
        if self.truncated {
            return "Checklist exceeds presentation limits; complete counts unavailable.".into();
        }
        let total: usize = self
            .sections
            .iter()
            .map(|section| section.tasks.len())
            .sum();
        if total == 0 {
            return "No checklist yet.".into();
        }
        let checked = self
            .sections
            .iter()
            .flat_map(|section| &section.tasks)
            .filter(|task| task.checked)
            .count();
        let mut summary = format!(
            "{checked}/{total} checked · ~{}% (checklist only, not goal completion)",
            checked * 100 / total
        );
        for section in &self.sections {
            if section.tasks.is_empty() {
                continue;
            }
            let checked = section.tasks.iter().filter(|task| task.checked).count();
            let _ = write!(
                summary,
                "\n{}: {checked}/{} checked",
                section.title,
                section.tasks.len()
            );
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_nested_items_but_not_fenced_examples() {
        let text = "- [x] outside\n\n## Phase 1: **Research**\n- [x] parent\n  - [ ] child\n\n```md\n- [x] example\n```\n";
        let checklist = Checklist::parse(text);
        assert_eq!(checklist.sections[0].tasks.len(), 1);
        assert_eq!(checklist.sections[1].title, "Phase 1: Research");
        assert_eq!(checklist.sections[1].tasks.len(), 2);
        assert_eq!(checklist.sections[1].tasks[1].depth, 1);
        let source = &checklist.sections[1].tasks[1].source;
        assert!(text[source.clone()].contains("[ ]"));
        assert!(checklist.summary().starts_with("2/3 checked · ~66%"));
    }

    #[test]
    fn phase_subheadings_and_task_labels_preserve_semantics() {
        let text = "## Phase 1: Build\n### Details\n- [x] **Parent** `code`\n  - [ ] child 🦀\n  - ordinary sibling\n\n## Verification\n- [ ] verify\n";
        let checklist = Checklist::parse(text);
        assert_eq!(checklist.sections.len(), 3);
        assert_eq!(checklist.sections[1].title, "Phase 1: Build");
        let tasks = &checklist.sections[1].tasks;
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].label, "Parent code");
        assert_eq!(tasks[1].label, "child 🦀");
        assert!(text[tasks[1].source.clone()].contains("child 🦀"));
        assert_eq!(checklist.sections[2].title, "Verification");
        assert_eq!(checklist.sections[2].tasks[0].label, "verify");
    }

    #[test]
    fn empty_and_oversized_are_not_zero_percent() {
        assert_eq!(Checklist::parse("No tasks").summary(), "No checklist yet.");
        for text in [
            "x".repeat(MAX_DOCUMENT_BYTES + 1),
            "- [x] task\n".repeat(MAX_TASKS + 1),
        ] {
            let checklist = Checklist::parse(&text);
            assert!(checklist.truncated);
            assert!(!checklist.summary().contains('%'));
        }
    }

    #[test]
    fn reopened_tasks_and_new_tasks_can_reduce_progress() {
        assert!(
            Checklist::parse("- [x] done")
                .summary()
                .starts_with("1/1 checked · ~100%")
        );
        assert!(
            Checklist::parse("- [ ] reopened\n- [ ] added")
                .summary()
                .starts_with("0/2 checked · ~0%")
        );
    }
}
