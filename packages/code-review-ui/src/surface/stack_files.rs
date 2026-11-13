//! Files across a stack's layers: one file through the stack (which layers change it, and how
//! much), and the overlap map (files several layers change: where a change low in the stack makes
//! restack conflicts, and where review attention compounds). Both are lists whose entries jump to
//! that layer's file.

use super::super::palette::{Overlay, OverlayEntry, OverlayKind};
use super::ReviewSurface;
use bcode_code_review::document::ItemKey;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::virtual_list::AnchorPolicy;
use std::collections::BTreeMap;

/// Action id prefix of an entry that jumps to a file of a layer (followed by the layer's source
/// id, a `\u{1}`, and the path).
pub const STACK_FILE_PREFIX: &str = "stack_file:";

/// One layer's change to one file.
struct Touch {
    source_id: String,
    layer: String,
    additions: u32,
    deletions: u32,
}

impl ReviewSurface {
    /// Every path the review's layers change, with each layer's change, in stack order.
    fn touches(&self) -> BTreeMap<String, Vec<Touch>> {
        let mut touches: BTreeMap<String, Vec<Touch>> = BTreeMap::new();
        let Some(review) = self.review.as_ref() else {
            return touches;
        };
        for group in review.doc.groups() {
            let Some(stack) = group.stack.filter(|stack| !stack.whole) else {
                continue;
            };
            let layer = stack.stack.layers.last().map_or_else(
                || group.label.to_owned(),
                |layer| format!("#{} {}", layer.number, layer.title),
            );
            for file in review.doc.group_files(group.index) {
                let Some(entry) = review.doc.entry(file) else {
                    continue;
                };
                touches.entry(file.path.clone()).or_default().push(Touch {
                    source_id: group.source.source_id.clone(),
                    layer: layer.clone(),
                    additions: entry.additions,
                    deletions: entry.deletions,
                });
            }
        }
        touches
    }

    fn touch_entry(path: &str, touch: &Touch) -> OverlayEntry {
        OverlayEntry {
            action: Some(format!("{STACK_FILE_PREFIX}{}\u{1}{path}", touch.source_id)),
            label: format!("{}  +{} −{}", touch.layer, touch.additions, touch.deletions),
            keys: String::new(),
        }
    }

    /// The file at the cursor through the stack: every layer that changes it.
    pub(super) fn open_file_through_stack(&mut self) {
        let Some(path) = self
            .review
            .as_ref()
            .and_then(super::Review::cursor_file)
            .map(|file| file.path.clone())
        else {
            self.set_status("No file at the cursor", StatusSeverity::Info);
            return;
        };
        let touches = self.touches();
        let Some(layers) = touches.get(&path).filter(|layers| !layers.is_empty()) else {
            self.set_status(
                "This file is not part of a stack's layers",
                StatusSeverity::Info,
            );
            return;
        };
        let mut entries = vec![OverlayEntry {
            action: None,
            label: format!(
                "{path}: changed by {} layer{}",
                layers.len(),
                if layers.len() == 1 { "" } else { "s" }
            ),
            keys: String::new(),
        }];
        entries.extend(layers.iter().map(|touch| Self::touch_entry(&path, touch)));
        self.overlay = Some(Overlay::new(OverlayKind::StackFile, entries, &self.theme));
    }

    /// The overlap map: files more than one layer changes, most layers first.
    pub(super) fn open_stack_overlaps(&mut self) {
        let touches = self.touches();
        let mut shared: Vec<(&String, &Vec<Touch>)> = touches
            .iter()
            .filter(|(_, layers)| layers.len() > 1)
            .collect();
        if shared.is_empty() {
            self.set_status(
                "No file is changed by more than one layer",
                StatusSeverity::Info,
            );
            return;
        }
        shared.sort_by_key(|(path, layers)| (std::cmp::Reverse(layers.len()), (*path).clone()));
        let mut entries = Vec::new();
        for (path, layers) in shared {
            entries.push(OverlayEntry {
                action: None,
                label: format!("{path}: {} layers", layers.len()),
                keys: String::new(),
            });
            entries.extend(layers.iter().map(|touch| Self::touch_entry(path, touch)));
        }
        self.overlay = Some(Overlay::new(OverlayKind::Overlaps, entries, &self.theme));
    }

    /// Run a [`STACK_FILE_PREFIX`] action: show that layer, at that file.
    pub(super) fn jump_to_stack_file(&mut self, target: &str) -> bool {
        let Some((source_id, path)) = target.split_once('\u{1}') else {
            return false;
        };
        self.rail_layer = Some(source_id.to_owned());
        self.show_stack_layer(Some(source_id));
        let Some(review) = self.review.as_mut() else {
            return false;
        };
        let Some(file) = review
            .doc
            .groups()
            .find(|group| group.source.source_id == source_id)
            .and_then(|group| {
                review
                    .doc
                    .group_files(group.index)
                    .find(|file| file.path == path)
                    .cloned()
            })
        else {
            return false;
        };
        let key = ItemKey::FileHeader(file);
        let Some(jump) = review.doc.jump_to(&key) else {
            return false;
        };
        review.set_cursor(jump.index, AnchorPolicy::ItemTopAtTop);
        review.pending_jump = jump.needs_load.map(|_| key);
        true
    }
}
