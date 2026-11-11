//! File panel model: the review's files as BMUX `tree_view` items (spec section 6.1).
//!
//! * The tree is built once per document from the file index: one group per source (or part)
//!   when there is more than one, directories in path order with single-directory chains
//!   compacted (`src/v2/tui`), and file rows in document order.
//! * File rows carry a status-colored label and right-aligned decorations, leftmost dropped
//!   first when narrow: stack layers, drafts, threads, a change bar, `+/−` counts, and the viewed
//!   mark. Directory and group rows carry aggregate threads, counts, and viewed progress.
//! * Decorations change only through [`FileTree::refresh`] for files whose header the document
//!   rebuilt (noted from splices), which updates the file's row and its ancestors' aggregates in
//!   place and advances [`FileTree::revision`], so the tree view's cached projection is reused
//!   across frames otherwise. Work per refresh is proportional to the tree depth.
//! * A filter keeps matching files and their ancestors; every word of the query must occur in
//!   the path (case-insensitive). While filtering, directories cannot collapse, so every match
//!   stays visible.

use super::rows::{change_bar, count_spans};
use super::theme::{ReviewTheme, status_letter};
use crate::v2::document::{FileRef, ItemKey, ReviewDocument, ReviewItem};
use bcode_code_review_models::{ReviewFileStatus, ReviewIndexFlag};
use bmux_tui::style::Modifier;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::tree_view::TreeViewItem;
use std::collections::BTreeMap;

/// Separator inside tree ids (never part of a path).
const SEP: char = '\u{1f}';

/// What a file row shows besides its index data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Decorations {
    viewed: bool,
    threads: u32,
    drafts: u32,
    layers: Vec<u64>,
}

/// Aggregates of a directory or group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    files: u32,
    additions: u64,
    deletions: u64,
    viewed: u32,
    threads: u32,
    drafts: u32,
}

#[derive(Debug, Clone)]
struct FileRow {
    file: FileRef,
    name: String,
    status: ReviewFileStatus,
    additions: u64,
    deletions: u64,
    binary: bool,
    decorations: Decorations,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Group { label: String, totals: Totals },
    Dir { name: String, totals: Totals },
    File(FileRow),
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    depth: u16,
    parent: Option<usize>,
    kind: NodeKind,
}

/// A directory while the tree is assembled: children in first-appearance order.
#[derive(Debug, Default)]
struct TrieDir {
    dirs: BTreeMap<String, usize>,
    children: Vec<TrieChild>,
}

#[derive(Debug)]
enum TrieChild {
    Dir(String, usize),
    File(FileRow),
}

/// The file panel's tree.
#[derive(Debug, Default)]
pub struct FileTree {
    nodes: Vec<Node>,
    items: Vec<TreeViewItem>,
    /// Node of each displayed item.
    shown: Vec<usize>,
    /// Displayed item of each node.
    item_of: Vec<Option<usize>>,
    files: BTreeMap<FileRef, usize>,
    ids: BTreeMap<String, usize>,
    revision: u64,
    query: Vec<String>,
    filter: String,
    matches: usize,
    /// Viewed files are left out (with directories that keep no file).
    hide_viewed: bool,
}

impl FileTree {
    /// The tree of every file in `doc`.
    #[must_use]
    pub fn new(doc: &ReviewDocument, theme: &ReviewTheme) -> Self {
        let mut groups: Vec<(String, Option<u32>, Vec<FileRow>)> = Vec::new();
        for file in doc.file_keys() {
            let Some(row) = file_row(doc, file) else {
                continue;
            };
            match groups.last_mut() {
                Some((source, part, rows)) if *source == file.source_id && *part == file.part => {
                    rows.push(row);
                }
                _ => groups.push((file.source_id.clone(), file.part, vec![row])),
            }
        }
        let grouped = groups.len() > 1;
        let mut tree = Self::default();
        for (source, part, rows) in groups {
            let depth = if grouped {
                let label = group_label(doc, &source, part);
                tree.push(
                    format!("g{SEP}{source}{SEP}{}", part_text(part)),
                    0,
                    None,
                    NodeKind::Group {
                        label,
                        totals: Totals::default(),
                    },
                );
                1
            } else {
                0
            };
            let parent = grouped.then(|| tree.nodes.len() - 1);
            let mut dirs = vec![TrieDir::default()];
            for row in rows {
                let mut dir = 0;
                let mut components: Vec<&str> = row.file.path.split('/').collect();
                components.pop();
                for component in components {
                    let existing = dirs[dir].dirs.get(component).copied();
                    let next = existing.unwrap_or_else(|| {
                        dirs.push(TrieDir::default());
                        let next = dirs.len() - 1;
                        dirs[dir].dirs.insert(component.to_owned(), next);
                        dirs[dir]
                            .children
                            .push(TrieChild::Dir(component.to_owned(), next));
                        next
                    });
                    dir = next;
                }
                dirs[dir].children.push(TrieChild::File(row));
            }
            let prefix = format!("{source}{SEP}{}{SEP}", part_text(part));
            tree.emit(&mut dirs, 0, depth, parent, &prefix, "");
        }
        tree.aggregate();
        tree.item_of = vec![None; tree.nodes.len()];
        tree.rebuild_items(theme);
        tree
    }

    fn push(&mut self, id: String, depth: u16, parent: Option<usize>, kind: NodeKind) -> usize {
        let index = self.nodes.len();
        if let NodeKind::File(row) = &kind {
            self.files.insert(row.file.clone(), index);
        }
        self.ids.insert(id.clone(), index);
        self.nodes.push(Node {
            id,
            depth,
            parent,
            kind,
        });
        index
    }

    /// Emit the children of trie directory `dir` in preorder, compacting chains of single
    /// directories into one row.
    fn emit(
        &mut self,
        dirs: &mut Vec<TrieDir>,
        dir: usize,
        depth: u16,
        parent: Option<usize>,
        prefix: &str,
        path: &str,
    ) {
        let children = std::mem::take(&mut dirs[dir].children);
        for child in children {
            match child {
                TrieChild::File(row) => {
                    let id = format!("f{SEP}{prefix}{}", row.file.path);
                    self.push(id, depth, parent, NodeKind::File(row));
                }
                TrieChild::Dir(mut name, mut next) => {
                    let mut full = join(path, &name);
                    while let [TrieChild::Dir(child, grandchild)] = dirs[next].children.as_slice() {
                        name = format!("{name}/{child}");
                        full = join(path, &name);
                        let grandchild = *grandchild;
                        dirs[next].children.clear();
                        next = grandchild;
                    }
                    let node = self.push(
                        format!("d{SEP}{prefix}{full}"),
                        depth,
                        parent,
                        NodeKind::Dir {
                            name,
                            totals: Totals::default(),
                        },
                    );
                    self.emit(
                        dirs,
                        next,
                        depth.saturating_add(1),
                        Some(node),
                        prefix,
                        &full,
                    );
                }
            }
        }
    }

    /// Recompute every directory and group aggregate.
    fn aggregate(&mut self) {
        for index in 0..self.nodes.len() {
            if let NodeKind::File(row) = &self.nodes[index].kind {
                let delta = file_totals(row);
                let mut parent = self.nodes[index].parent;
                while let Some(ancestor) = parent {
                    if let NodeKind::Group { totals, .. } | NodeKind::Dir { totals, .. } =
                        &mut self.nodes[ancestor].kind
                    {
                        add_totals(totals, delta, 1);
                    }
                    parent = self.nodes[ancestor].parent;
                }
            }
        }
    }

    /// Displayed tree items.
    #[must_use]
    pub fn items(&self) -> &[TreeViewItem] {
        &self.items
    }

    /// Revision of [`Self::items`]; changes whenever any displayed item changes.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Ids of every directory and group, for the initial (fully expanded) state.
    pub fn expandable_ids(&self) -> impl Iterator<Item = &str> {
        self.nodes
            .iter()
            .filter(|node| !matches!(node.kind, NodeKind::File(_)))
            .map(|node| node.id.as_str())
    }

    /// Files in the tree.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Files matching the current filter (every file without one).
    #[must_use]
    pub const fn matches(&self) -> usize {
        self.matches
    }

    /// The current filter text.
    #[must_use]
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Tree id of `file`'s row.
    #[must_use]
    pub fn file_id(&self, file: &FileRef) -> Option<&str> {
        self.files
            .get(file)
            .map(|index| self.nodes[*index].id.as_str())
    }

    /// The file of a file row id.
    #[must_use]
    pub fn file_of(&self, id: &str) -> Option<&FileRef> {
        match &self.nodes[*self.ids.get(id)?].kind {
            NodeKind::File(row) => Some(&row.file),
            _ => None,
        }
    }

    /// Whether `id` is a directory or group row.
    #[must_use]
    pub fn is_expandable(&self, id: &str) -> bool {
        self.ids
            .get(id)
            .is_some_and(|index| !matches!(self.nodes[*index].kind, NodeKind::File(_)))
    }

    /// The first file under the directory or group `id`, in tree order.
    #[must_use]
    pub fn first_file_under(&self, id: &str) -> Option<&FileRef> {
        let start = *self.ids.get(id)?;
        let depth = self.nodes[start].depth;
        self.nodes[start + 1..]
            .iter()
            .take_while(|node| node.depth > depth)
            .find_map(|node| match &node.kind {
                NodeKind::File(row) => Some(&row.file),
                _ => None,
            })
    }

    /// Ids of the ancestors of `id`, outermost first.
    #[must_use]
    pub fn ancestors(&self, id: &str) -> Vec<&str> {
        let mut ancestors = Vec::new();
        let mut parent = self.ids.get(id).and_then(|index| self.nodes[*index].parent);
        while let Some(index) = parent {
            ancestors.push(self.nodes[index].id.as_str());
            parent = self.nodes[index].parent;
        }
        ancestors.reverse();
        ancestors
    }

    /// Show only files matching `query`. Returns whether the shown items changed.
    pub fn set_filter(&mut self, query: &str, theme: &ReviewTheme) -> bool {
        if query == self.filter {
            return false;
        }
        query.clone_into(&mut self.filter);
        self.query = query.split_whitespace().map(str::to_lowercase).collect();
        self.rebuild_items(theme);
        true
    }

    /// Show or hide viewed files. Returns whether the shown items changed.
    pub fn set_hide_viewed(&mut self, hide: bool, theme: &ReviewTheme) -> bool {
        if hide == self.hide_viewed {
            return false;
        }
        self.hide_viewed = hide;
        self.rebuild_items(theme);
        true
    }

    /// Whether viewed files are hidden.
    #[must_use]
    pub const fn hides_viewed(&self) -> bool {
        self.hide_viewed
    }

    /// Rebuild every displayed item (after a filter, the hide-viewed toggle, or a theme change).
    pub fn rebuild_items(&mut self, theme: &ReviewTheme) {
        let filtering = !self.query.is_empty();
        let mut keep = vec![false; self.nodes.len()];
        let mut matches = 0;
        for (index, node) in self.nodes.iter().enumerate() {
            let NodeKind::File(row) = &node.kind else {
                continue;
            };
            if self.hide_viewed && row.decorations.viewed {
                continue;
            }
            let path = row.file.path.to_lowercase();
            if self.query.iter().all(|word| path.contains(word.as_str())) {
                matches += 1;
                keep[index] = true;
                let mut parent = node.parent;
                while let Some(ancestor) = parent {
                    if keep[ancestor] {
                        break;
                    }
                    keep[ancestor] = true;
                    parent = self.nodes[ancestor].parent;
                }
            }
        }
        self.matches = matches;
        self.shown.clear();
        self.items.clear();
        self.item_of = vec![None; self.nodes.len()];
        for (index, keep) in keep.into_iter().enumerate() {
            if keep {
                self.item_of[index] = Some(self.items.len());
                self.shown.push(index);
                self.items.push(self.item(index, theme, filtering));
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// Re-read `file`'s decorations from `doc`; updates its row and its ancestors' aggregates
    /// when they changed. Returns whether anything changed.
    pub fn refresh(&mut self, doc: &ReviewDocument, file: &FileRef, theme: &ReviewTheme) -> bool {
        let Some(&index) = self.files.get(file) else {
            return false;
        };
        let Some(decorations) = header(doc, file).map(decorations_of) else {
            return false;
        };
        let NodeKind::File(row) = &mut self.nodes[index].kind else {
            return false;
        };
        if row.decorations == decorations {
            return false;
        }
        let membership = self.hide_viewed && row.decorations.viewed != decorations.viewed;
        let before = file_totals(row);
        row.decorations = decorations;
        let after = file_totals(row);
        let filtering = !self.query.is_empty();
        let mut changed = vec![index];
        let mut parent = self.nodes[index].parent;
        while let Some(ancestor) = parent {
            if let NodeKind::Group { totals, .. } | NodeKind::Dir { totals, .. } =
                &mut self.nodes[ancestor].kind
            {
                add_totals(totals, before, -1);
                add_totals(totals, after, 1);
            }
            changed.push(ancestor);
            parent = self.nodes[ancestor].parent;
        }
        if membership {
            // The file joins or leaves the shown items.
            self.rebuild_items(theme);
            return true;
        }
        for node in changed {
            if let Some(item) = self.item_of[node] {
                self.items[item] = self.item(node, theme, filtering);
            }
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn item(&self, index: usize, theme: &ReviewTheme, filtering: bool) -> TreeViewItem {
        let node = &self.nodes[index];
        let icons = &theme.icons;
        match &node.kind {
            NodeKind::File(row) => {
                let name_style = if row.decorations.viewed {
                    theme.path_viewed
                } else if row.status == ReviewFileStatus::Deleted {
                    theme.text.add_modifier(Modifier::CROSSED_OUT)
                } else {
                    theme.text
                };
                let label = Line::from_spans(vec![
                    Span::styled(status_letter(row.status), theme.status(row.status)),
                    Span::raw(" "),
                    Span::styled(row.name.clone(), name_style),
                ]);
                let decorations = &row.decorations;
                let mut trailing = Vec::new();
                if !decorations.layers.is_empty() {
                    let layers: Vec<String> = decorations
                        .layers
                        .iter()
                        .map(|layer| format!("#{layer}"))
                        .collect();
                    trailing.push(Line::from_spans(vec![Span::styled(
                        layers.join(" "),
                        theme.badge_open,
                    )]));
                }
                if decorations.drafts > 0 {
                    trailing.push(badge(icons.draft, decorations.drafts, theme.badge_draft));
                }
                if decorations.threads > 0 {
                    trailing.push(badge(icons.thread, decorations.threads, theme.badge_open));
                }
                if row.binary {
                    trailing.push(Line::from_spans(vec![Span::styled(
                        "bin",
                        theme.status_binary,
                    )]));
                } else {
                    trailing.push(Line::from_spans(change_bar(
                        row.additions,
                        row.deletions,
                        theme,
                    )));
                    trailing.push(Line::from_spans(count_spans(
                        row.additions,
                        row.deletions,
                        theme,
                    )));
                }
                trailing.push(Line::from_spans(vec![if decorations.viewed {
                    Span::styled(icons.viewed, theme.viewed)
                } else {
                    Span::styled(icons.unviewed, theme.muted)
                }]));
                TreeViewItem::rich(node.id.clone(), label, node.depth).trailing(trailing)
            }
            NodeKind::Dir { name, totals }
            | NodeKind::Group {
                label: name,
                totals,
            } => {
                // Directories end with a slash; source groups are named, not paths.
                let label = if matches!(node.kind, NodeKind::Group { .. }) {
                    Line::from_spans(vec![Span::styled(name.clone(), theme.source_header)])
                } else {
                    Line::from_spans(vec![Span::styled(format!("{name}/"), theme.directory)])
                };
                let mut trailing = Vec::new();
                if totals.drafts > 0 {
                    trailing.push(badge(icons.draft, totals.drafts, theme.badge_draft));
                }
                if totals.threads > 0 {
                    trailing.push(badge(icons.thread, totals.threads, theme.badge_open));
                }
                trailing.push(Line::from_spans(count_spans(
                    totals.additions,
                    totals.deletions,
                    theme,
                )));
                trailing.push(if totals.files > 0 && totals.viewed == totals.files {
                    Line::from_spans(vec![Span::styled(
                        format!("{} {}", icons.viewed, totals.files),
                        theme.viewed,
                    )])
                } else {
                    Line::from_spans(vec![Span::styled(
                        format!("{}/{}", totals.viewed, totals.files),
                        theme.muted,
                    )])
                });
                TreeViewItem::rich(node.id.clone(), label, node.depth)
                    .trailing(trailing)
                    .expandable(!filtering)
            }
        }
    }
}

fn badge(icon: &str, count: u32, style: bmux_tui::style::Style) -> Line {
    Line::from_spans(vec![Span::styled(format!("{icon}{count}"), style)])
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}/{name}")
    }
}

fn part_text(part: Option<u32>) -> String {
    part.map_or_else(String::new, |part| part.to_string())
}

fn header<'a>(
    doc: &'a ReviewDocument,
    file: &FileRef,
) -> Option<&'a crate::v2::document::FileHeaderItem> {
    let index = doc.index_of(&ItemKey::FileHeader(file.clone()))?;
    match doc.item(index)? {
        ReviewItem::FileHeader(header) => Some(header),
        _ => None,
    }
}

fn decorations_of(header: &crate::v2::document::FileHeaderItem) -> Decorations {
    Decorations {
        viewed: header.viewed,
        threads: u32::try_from(header.thread_count).unwrap_or(u32::MAX),
        // Draft counts arrive with the annotation pipeline (spec phase 3c).
        drafts: 0,
        layers: header.layers.clone(),
    }
}

fn file_row(doc: &ReviewDocument, file: &FileRef) -> Option<FileRow> {
    let header = header(doc, file)?;
    let name = file
        .path
        .rsplit_once('/')
        .map_or(file.path.as_str(), |(_, name)| name)
        .to_owned();
    Some(FileRow {
        file: file.clone(),
        name,
        status: header.status,
        additions: u64::from(header.additions),
        deletions: u64::from(header.deletions),
        binary: header.flags.contains(&ReviewIndexFlag::Binary),
        decorations: decorations_of(header),
    })
}

fn group_label(doc: &ReviewDocument, source: &str, part: Option<u32>) -> String {
    let key = ItemKey::SourceHeader {
        source_id: source.to_owned(),
        part,
    };
    doc.index_of(&key)
        .and_then(|index| match doc.item(index) {
            Some(ReviewItem::SourceHeader(header)) => Some(header.label.clone()),
            _ => None,
        })
        .unwrap_or_else(|| source.to_owned())
}

fn file_totals(row: &FileRow) -> Totals {
    Totals {
        files: 1,
        additions: row.additions,
        deletions: row.deletions,
        viewed: u32::from(row.decorations.viewed),
        threads: row.decorations.threads,
        drafts: row.decorations.drafts,
    }
}

fn add_totals(totals: &mut Totals, delta: Totals, sign: i8) {
    let apply32 = |value: &mut u32, delta: u32| {
        *value = if sign < 0 {
            value.saturating_sub(delta)
        } else {
            value.saturating_add(delta)
        };
    };
    let apply64 = |value: &mut u64, delta: u64| {
        *value = if sign < 0 {
            value.saturating_sub(delta)
        } else {
            value.saturating_add(delta)
        };
    };
    apply32(&mut totals.files, delta.files);
    apply64(&mut totals.additions, delta.additions);
    apply64(&mut totals.deletions, delta.deletions);
    apply32(&mut totals.viewed, delta.viewed);
    apply32(&mut totals.threads, delta.threads);
    apply32(&mut totals.drafts, delta.drafts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::document::DocumentConfig;
    use crate::v2::document::tests::support::{Fake, SynthFile};
    use crate::v2::testing::harness::paint;
    use crate::v2::tui::theme::IconStyle;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::geometry::Size;
    use bmux_tui::paint::LocalRect;
    use bmux_tui_components::tree_view::{
        TreeViewComponent, TreeViewLabelOverflow, TreeViewState, TreeViewTrailingLayout,
    };
    use std::cell::RefCell;

    fn sample() -> Fake {
        Fake::new(vec![
            SynthFile::added("a/b/c/deep.rs", 12),
            SynthFile::added("a/b/c/other.rs", 3),
            SynthFile::deleted("漢字/ファイル名.rs", 40),
            SynthFile::added("emoji/🦀🦀.rs", 5),
            SynthFile::added("combining/e\u{301}te\u{301}.txt", 2),
            SynthFile::binary("assets/logo.png"),
            SynthFile::added("top.rs", 1),
        ])
    }

    #[test]
    fn directories_compact_and_aggregate_their_files() {
        let doc = sample().document(DocumentConfig::default());
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        let labels: Vec<(u16, String)> = tree
            .items()
            .iter()
            .map(|item| (item.depth, item.label.clone()))
            .collect();
        // `a/b/c` has a single directory chain: one row.
        assert!(labels.contains(&(0, "a/b/c/".to_owned())), "{labels:?}");
        assert!(labels.contains(&(1, "A deep.rs".to_owned())), "{labels:?}");
        let dir = tree
            .items()
            .iter()
            .find(|item| item.label == "a/b/c/")
            .unwrap();
        let trailing: Vec<String> = dir.trailing.iter().map(Line::plain_text).collect();
        assert_eq!(trailing, ["+15 −0", "0/2"]);
        assert_eq!(tree.file_count(), 7);
        let id = tree
            .file_id(&doc.file_keys().next().unwrap().clone())
            .unwrap();
        assert!(tree.file_of(id).is_some());
        assert_eq!(
            tree.ancestors(id).len(),
            usize::from(
                tree.items()
                    .iter()
                    .find(|item| item.id == id)
                    .unwrap()
                    .depth
            )
        );
    }

    #[test]
    fn filter_keeps_matches_and_their_directories() {
        let doc = sample().document(DocumentConfig::default());
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut tree = FileTree::new(&doc, &theme);
        let revision = tree.revision();
        assert!(tree.set_filter("DEEP", &theme));
        assert_ne!(tree.revision(), revision);
        let labels: Vec<&str> = tree
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["a/b/c/", "A deep.rs"]);
        assert_eq!(tree.matches(), 1);
        // Directories cannot collapse while filtering.
        assert!(!tree.items()[0].expandable);
        assert!(!tree.set_filter("DEEP", &theme));
        tree.set_filter("", &theme);
        assert_eq!(tree.matches(), 7);
        assert!(tree.items().iter().any(|item| item.expandable));
    }

    #[test]
    fn rows_stay_inside_tiny_widths_with_wide_and_combining_names() {
        let doc = sample().document(DocumentConfig::default());
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        let state = RefCell::new(TreeViewState::new(None));
        state
            .borrow_mut()
            .set_expanded_ids(tree.expandable_ids().map(str::to_owned));
        let component = TreeViewComponent::new("tree", tree.items(), &state)
            .items_revision(tree.revision())
            .trailing_layout(
                TreeViewTrailingLayout::new()
                    .min_label_width(10)
                    .label_overflow(TreeViewLabelOverflow::Ellipsis),
            );
        for width in [0_u16, 1, 2, 3, 5, 8, 13, 21, 34, 60] {
            let height = u16::try_from(tree.items().len()).unwrap();
            let screen = paint(width + 2, height, |area, cx| {
                let inner = bmux_tui::geometry::Rect::new(1, 0, width, area.height);
                let layout = component.layout(
                    Constraints::tight(Size::new(width, area.height)),
                    &mut LayoutCx::new(),
                );
                cx.with_child(
                    1,
                    0,
                    LocalRect::new(0, 0, inner.width, inner.height),
                    |cx| component.paint(&layout, cx),
                );
            });
            for y in 0..height {
                // Nothing painted outside the tree's columns, and wide graphemes are never cut.
                assert_eq!(screen.cell(0, y).unwrap().symbol, " ", "{width}: {y}");
                assert_eq!(
                    screen.cell(width + 1, y).unwrap().symbol,
                    " ",
                    "{width}: {y}"
                );
                let row = screen.row_text(y);
                assert!(!row.contains('\u{fffd}'), "{width}: {row:?}");
            }
            for hit in screen.hits() {
                assert!(
                    hit.area.x >= 1 && hit.area.right() <= width + 1,
                    "{width}: {hit:?}"
                );
            }
            if width >= 34 {
                let text = screen.text();
                assert!(text.contains("ファイル"), "{text}");
                assert!(text.contains('🦀'), "{text}");
                assert!(text.contains("e\u{301}te\u{301}"), "{text}");
            }
        }
    }
}
