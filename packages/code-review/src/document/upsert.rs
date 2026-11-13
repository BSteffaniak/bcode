//! Adding or replacing one source group of a [`ReviewDocument`] in place.
//!
//! A group fills in (a loading layer listed) or is re-pinned (its commits moved) while the
//! reviewer reads another: the document is rebuilt around the new group with every other file's
//! state carried over by key (loaded hunks, revealed lines, viewed state, loads in flight), and
//! the list sees one splice covering only the items that changed, so the cursor, the scroll
//! position, and every other group stay as they were.

use super::fenwick::Fenwick;
use super::review::{Content, Pending, ReviewDocument, SourceInput, Splice};
use std::collections::{BTreeMap, BTreeSet};

/// The groups of a document after one is added or replaced.
struct Regrouped {
    inputs: Vec<SourceInput>,
    hidden: Vec<bool>,
    /// Index of the added or replaced group.
    position: usize,
    /// The replaced group, when its revisions changed: its files start over.
    fresh_group: Option<usize>,
}

impl ReviewDocument {
    /// Add `input`'s group, or replace the group of the same source (and part), and report the
    /// change as one splice. A new group goes at group position `at` (clamped), so callers keep
    /// workspace order. A replaced group keeps its files' loaded state when its revisions did
    /// not change; with new revisions, its files start unloaded. Returns whether anything
    /// changed.
    pub fn upsert_group(&mut self, input: SourceInput, at: usize) -> bool {
        let Some(regrouped) = self.regroup(input, at) else {
            return false;
        };
        let mut rebuilt = Self::structure(self.repo_path.clone(), regrouped.inputs, self.config);
        rebuilt.layout = self.layout;
        rebuilt.ignore_whitespace = self.ignore_whitespace;
        rebuilt.composer.clone_from(&self.composer);
        rebuilt.marks = std::mem::take(&mut self.marks);
        rebuilt.tick = self.tick;
        rebuilt.next_ticket = self.next_ticket;
        for (group, hidden) in rebuilt.groups.iter_mut().zip(regrouped.hidden) {
            group.hidden = hidden;
        }
        let moved = self.carry_files_into(&mut rebuilt, regrouped.fresh_group);
        rebuilt.build_all();
        // Files that did not carry their state learn it from the marks; annotations are placed
        // again where they belong now.
        let carried: BTreeSet<usize> = moved.values().copied().collect();
        for file in 0..rebuilt.files.len() {
            if !carried.contains(&file) {
                rebuilt.refresh_viewed(file);
            }
        }
        let header = rebuilt.groups[regrouped.position].header_block;
        rebuilt.rebuild_block(header);
        if let Some(annotations) = self.annotations_input.take() {
            rebuilt.set_annotations(&annotations);
        }
        rebuilt.splices = std::mem::take(&mut self.splices);
        rebuilt.revision = self.revision;
        if let Some((start, removed, inserted)) = self.changed_items(&rebuilt) {
            rebuilt.revision += 1;
            rebuilt.splices.push(Splice {
                start,
                removed,
                inserted,
                revision: rebuilt.revision,
            });
        }
        *self = rebuilt;
        true
    }

    /// The groups with `input`'s added (at `at`) or replacing the group of its source; `None`
    /// when that group already is `input`.
    fn regroup(&self, input: SourceInput, at: usize) -> Option<Regrouped> {
        let part = |source: &bcode_code_review_models::ResolvedSource| {
            source.part.as_ref().map(|part| part.index)
        };
        let replaced = self.groups.iter().position(|group| {
            group.source.source_id == input.source.source_id
                && part(&group.source) == part(&input.source)
        });
        // Files of the replaced group keep their state only when it shows the same revisions.
        let fresh_group = replaced.filter(|&index| self.groups[index].source != input.source);
        let mut inputs: Vec<SourceInput> = self
            .groups
            .iter()
            .map(|group| SourceInput {
                source: group.source.clone(),
                label: group.label.clone(),
                entries: self.files[group.files.clone()]
                    .iter()
                    .map(|file| file.entry.clone())
                    .collect(),
                stack: group.stack.clone(),
                loading: group.loading,
            })
            .collect();
        let mut hidden: Vec<bool> = self.groups.iter().map(|group| group.hidden).collect();
        let position = if let Some(index) = replaced {
            if inputs[index] == input {
                return None;
            }
            inputs[index] = input;
            index
        } else {
            let index = at.min(inputs.len());
            inputs.insert(index, input);
            // A new group is hidden when the document shows one other source alone.
            hidden.insert(index, self.focus().is_some());
            index
        };
        Some(Regrouped {
            inputs,
            hidden,
            position,
            fresh_group,
        })
    }

    /// Move every kept file's state (by key) into `rebuilt`, remap loads in flight, and derive
    /// the residency and viewed counts. Returns the old file index of each moved file → its new
    /// one.
    fn carry_files_into(
        &mut self,
        rebuilt: &mut Self,
        fresh_group: Option<usize>,
    ) -> BTreeMap<usize, usize> {
        let mut moved = BTreeMap::new();
        for (old_index, mut old) in std::mem::take(&mut self.files).into_iter().enumerate() {
            if fresh_group == Some(old.group) {
                continue;
            }
            let Some(&new_index) = rebuilt.file_index.get(old.key.as_ref()) else {
                continue;
            };
            let state = &mut rebuilt.files[new_index];
            if state.entry != old.entry {
                continue;
            }
            state.content = std::mem::replace(&mut old.content, Content::Unloaded);
            state.revealed = old.revealed;
            state.line_count = old.line_count;
            state.patch_id = old.patch_id;
            state.content_oid = old.content_oid;
            state.viewed = old.viewed;
            state.attribution = old.attribution;
            state.last_used = old.last_used;
            state.resident_bytes = old.resident_bytes;
            state.carried_over = old.carried_over;
            moved.insert(old_index, new_index);
        }
        rebuilt.pending = std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|(ticket, pending)| {
                let remapped = match pending {
                    Pending::Hunks(file) => Pending::Hunks(*moved.get(&file)?),
                    Pending::Attribution(file) => Pending::Attribution(*moved.get(&file)?),
                    Pending::Blob { file, start, count } => Pending::Blob {
                        file: *moved.get(&file)?,
                        start,
                        count,
                    },
                };
                Some((ticket, remapped))
            })
            .collect();
        for (index, file) in rebuilt.files.iter().enumerate() {
            rebuilt.resident_bytes += file.resident_bytes;
            if file.loaded().is_some() {
                rebuilt.lru.insert((file.last_used, index));
            }
            if file.viewed.is_viewed() {
                rebuilt.viewed_count += 1;
                rebuilt.groups[file.group].viewed += 1;
            }
        }
        moved
    }

    /// The items that differ between this document and `rebuilt`, as `(start, removed,
    /// inserted)`: everything between the unchanged items before and after. `None` when none do.
    fn changed_items(&self, rebuilt: &Self) -> Option<(usize, usize, usize)> {
        let old: Vec<_> = self.blocks.iter().flat_map(|block| &block.items).collect();
        let new: Vec<_> = rebuilt
            .blocks
            .iter()
            .flat_map(|block| &block.items)
            .collect();
        let prefix = old
            .iter()
            .zip(&new)
            .take_while(|(old, new)| old == new)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let removed = old.len() - prefix - suffix;
        let inserted = new.len() - prefix - suffix;
        (removed > 0 || inserted > 0).then_some((prefix, removed, inserted))
    }

    /// Build every block's items and the length and row sums from the files and groups.
    pub(super) fn build_all(&mut self) {
        for block in 0..self.blocks.len() {
            let built = self.build_block(block);
            self.blocks[block].lookup = super::review::BlockLookup::build(&built.items);
            self.blocks[block].items = built.items;
            self.blocks[block].hunk_parent = built.hunk_parent;
        }
        let lengths: Vec<u64> = self
            .blocks
            .iter()
            .map(|block| block.items.len() as u64)
            .collect();
        let rows: Vec<u64> = self.blocks.iter().map(super::build::block_rows).collect();
        self.lengths = Fenwick::from_values(&lengths);
        self.rows = Fenwick::from_values(&rows);
    }
}
