//! Bounded, read-only projection of goal working-document checklists.

use std::fmt::Write as _;
use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

const MAX_DOCUMENT_BYTES: usize = 65_536;
const MAX_TASKS: usize = 1_024;
const MAX_SUMMARY_SECTIONS: usize = 8;

/// Mechanical checklist state, never an execution outcome.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecklistState {
    NoChecklist,
    Unchecked,
    PartiallyChecked,
    AllChecked,
}

impl Section {
    fn state(&self) -> ChecklistState {
        let checked = self.tasks.iter().filter(|task| task.checked).count();
        match (checked, self.tasks.len()) {
            (_, 0) => ChecklistState::NoChecklist,
            (0, _) => ChecklistState::Unchecked,
            (checked, total) if checked == total => ChecklistState::AllChecked,
            _ => ChecklistState::PartiallyChecked,
        }
    }
}

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
    /// Whether this heading explicitly identifies a phase, not an inferred active stage.
    #[serde(default)]
    pub is_phase: bool,
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
                is_phase: false,
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
                                is_phase,
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
        let checked = self
            .sections
            .iter()
            .flat_map(|section| &section.tasks)
            .filter(|task| task.checked)
            .count();
        let mut summary = (checked * 100).checked_div(total).map_or_else(
            || "No checklist yet.".into(),
            |percentage| format!("{checked}/{total} checked · ~{percentage}% (checklist only, not goal completion)"),
        );
        if let Some(section) = self
            .sections
            .iter()
            .find(|section| section.is_phase && section.tasks.iter().any(|task| !task.checked))
        {
            let _ = write!(summary, "\nNext unchecked phase: {}", section.title);
        }
        let sections: Vec<_> = self
            .sections
            .iter()
            .filter(|section| section.is_phase || !section.tasks.is_empty())
            .collect();
        for section in sections.iter().take(MAX_SUMMARY_SECTIONS) {
            let checked = section.tasks.iter().filter(|task| task.checked).count();
            let state = match section.state() {
                ChecklistState::NoChecklist => "no checklist",
                ChecklistState::Unchecked => "unchecked",
                ChecklistState::PartiallyChecked => "partially checked",
                ChecklistState::AllChecked => "all checked",
            };
            let _ = write!(
                summary,
                "\n{}: {checked}/{} · {state}",
                section.title,
                section.tasks.len()
            );
        }
        if sections.len() > MAX_SUMMARY_SECTIONS {
            let _ = write!(
                summary,
                "\n{} more checklist sections; counts above include all sections.",
                sections.len() - MAX_SUMMARY_SECTIONS
            );
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_phases_are_visible_without_inventing_completion() {
        let empty = Checklist::parse("## Phase 1\nPlanning prose\n## Phase 2\n").summary();
        assert!(empty.starts_with("No checklist yet."));
        assert!(empty.contains("Phase 1: 0/0 · no checklist"));
        assert!(empty.contains("Phase 2: 0/0 · no checklist"));
        assert!(!empty.contains('%'));
        assert!(!empty.contains("Next unchecked phase:"));
        let mixed = Checklist::parse("## Phase 1\n## Phase 2\n- [x] done\n- [ ] todo\n").summary();
        assert!(mixed.starts_with("1/2 checked · ~50%"));
        assert!(mixed.contains("Phase 1: 0/0 · no checklist"));
        assert!(mixed.contains("Next unchecked phase: Phase 2"));
    }

    #[test]
    fn next_unchecked_phase_excludes_prose_groups_and_empty_phases() {
        let checklist = Checklist::parse(
            "# Notes\n- [ ] outside\n## Phase 1\nno tasks\n## Phase 2\n- [x] done\n## Phase 3\n### Detail\n- [ ] remaining\n## Phase 4\n- [ ] later\n",
        );
        assert!(!checklist.sections[1].is_phase);
        assert!(checklist.sections[2].is_phase);
        assert_eq!(checklist.sections[2].state(), ChecklistState::NoChecklist);
        assert!(
            checklist
                .summary()
                .contains("Next unchecked phase: Phase 3")
        );
        let reopened =
            Checklist::parse("## Phase 2\n- [ ] reopened\n## Phase 3\n- [ ] remaining\n");
        assert!(reopened.summary().contains("Next unchecked phase: Phase 2"));
        for text in ["## Phase 1\n- [x] done", "# Notes\n- [ ] outside"] {
            assert!(
                !Checklist::parse(text)
                    .summary()
                    .contains("Next unchecked phase:")
            );
        }
    }

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
    fn states_and_summary_limits_do_not_change_total_counts() {
        for (source, state) in [
            ("## Empty", ChecklistState::NoChecklist),
            ("- [ ] open", ChecklistState::Unchecked),
            ("- [x] done", ChecklistState::AllChecked),
            ("- [x] done\n- [ ] open", ChecklistState::PartiallyChecked),
        ] {
            let checklist = Checklist::parse(source);
            assert_eq!(checklist.sections.last().unwrap().state(), state);
        }
        let mut text = String::new();
        for index in 0..12 {
            writeln!(text, "## Phase {index}\n- [x] done").unwrap();
        }
        let summary = Checklist::parse(&text).summary();
        assert!(summary.starts_with("12/12 checked · ~100%"));
        assert!(summary.contains("4 more checklist sections"));
        assert_eq!(summary.lines().count(), 10);
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
