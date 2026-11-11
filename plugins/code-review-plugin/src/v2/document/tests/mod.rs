//! Behavior tests for the review document.

mod annotations;
mod benchmark;
mod expansion;
mod integration;
mod navigation;
mod sequence;
mod sources;
pub mod support;

use super::{LineContent, ReviewDocument, ReviewItem};
use support::{Fake, SynthFile};

/// Assert that every line and row shows the text of its own line number on each side.
pub fn assert_lines_match(doc: &ReviewDocument, fake: &Fake) {
    for index in 0..doc.len() {
        let item = doc.item(index).expect("item");
        let Some(file) = item.file() else { continue };
        let synth: &SynthFile = &fake.files[&file.path];
        let check = |old: Option<u32>, new: Option<u32>, content: &LineContent| {
            let Some(text) = content.text() else { return };
            if let (Some(line), Some(lines)) = (old, synth.old.as_ref()) {
                assert_eq!(
                    lines[(line - 1) as usize],
                    text,
                    "old line {line} of {}",
                    file.path
                );
            }
            if let (Some(line), Some(lines)) = (new, synth.new.as_ref()) {
                assert_eq!(
                    lines[(line - 1) as usize],
                    text,
                    "new line {line} of {}",
                    file.path
                );
            }
        };
        match item {
            ReviewItem::Line(line) => check(line.old_line, line.new_line, &line.content),
            ReviewItem::LinePair(pair) => {
                if let Some(side) = &pair.old {
                    check(Some(side.line), None, &side.content);
                }
                if let Some(side) = &pair.new {
                    check(None, Some(side.line), &side.content);
                }
            }
            _ => {}
        }
    }
}
