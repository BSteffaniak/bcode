//! File panel model: the review's files as BMUX `tree_view` items.
//!
//! * The tree is built once per document from the file index: one group per source (or part)
//!   when there is more than one, directories in path order with single-directory chains
//!   compacted (`src/review/tui`), and file rows in document order.
//! * Flattened ([`FileLayout::Flat`]), the panel lists files without directories, each by its
//!   whole path (the directories dimmed, and the first to give way when the row is narrow), in
//!   document or path order ([`FlatOrder`]). Source groups stay unless merged
//!   ([`FlatLayout::merge_groups`]); merged rows name their source at their end, since layers of
//!   a stack can change the same path. File ids are the same in both layouts, so selection and
//!   the open file carry across a switch.
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
//! * Under a comparison the tree says how each file changed since the baseline, the way the
//!   document shows it: changed and new files are marked and their since counts shown,
//!   unchanged files are dimmed, files the comparison hides in the document (unchanged files in
//!   the changes view, every file of a layer only rebased) are left out and counted on their
//!   group, and files the baseline's change had that the current one dropped are listed, struck
//!   through and inert, at the end of their group. Group rows say what the source's comparison
//!   found; a compared group shows even when the comparison hides all its files.

use super::rows::{change_bar, count_spans};
use super::theme::{ReviewTheme, status_letter};
use bcode_code_review::document::{FileRef, ItemKey, ReviewDocument, ReviewItem};
use bcode_code_review_models::{DeltaKind, FileSince, ReviewFileStatus, ReviewIndexFlag};
use bmux_tui::style::Modifier;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::tree_view::TreeViewItem;
use std::collections::BTreeMap;

/// Separator inside tree ids (never part of a path).
const SEP: char = '\u{1f}';

/// How the file panel lists files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileLayout {
    /// Directories and the files in them.
    #[default]
    Tree,
    /// Files by their whole paths, without directories.
    Flat(FlatLayout),
}

impl FileLayout {
    /// Whether files are listed flat.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        matches!(self, Self::Flat(_))
    }
}

/// How a flat file list is arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlatLayout {
    /// Order of the files.
    pub order: FlatOrder,
    /// One list for every source instead of one per source (or part).
    pub merge_groups: bool,
}

/// Order of a flat file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlatOrder {
    /// The document's order, so moving down the list follows the review.
    #[default]
    Document,
    /// By path.
    Path,
}

/// What the file panel shows besides the document, as the reviewer chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilePanelView {
    /// Viewed files are left out.
    pub hide_viewed: bool,
    /// Tree or flat list.
    pub layout: FileLayout,
    /// The reviewer's flat (`Some(true)`) or tree choice for this review, saved with it; unset
    /// follows the configuration.
    pub flat_choice: Option<bool>,
}

/// What a file row shows besides its index data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Decorations {
    viewed: bool,
    threads: u32,
    drafts: u32,
    layers: Vec<u64>,
    /// How the file changed since its source's baseline, when compared.
    since: Option<FileSince>,
}

/// What a group's comparison found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GroupCompare {
    /// What it found; `None` while it computes.
    kind: Option<DeltaKind>,
    added: u64,
    removed: u64,
    /// Files changed (or new, or dropped) since the baseline.
    changed_files: usize,
    /// Files the comparison hides in the document, left out of the tree.
    hidden: usize,
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
    /// The row's source, named at its end when a flat list merges sources.
    source_tag: Option<String>,
    status: ReviewFileStatus,
    additions: u64,
    deletions: u64,
    binary: bool,
    decorations: Decorations,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Group {
        label: String,
        totals: Totals,
        compare: Option<GroupCompare>,
    },
    Dir {
        name: String,
        /// Repository-relative path of the directory.
        path: String,
        totals: Totals,
    },
    File(FileRow),
    /// A file the baseline's change had that the current change dropped (inert).
    Gone {
        path: String,
    },
}

/// A group while the tree is assembled.
struct GroupRows {
    source: String,
    part: Option<u32>,
    rows: Vec<FileRow>,
    hidden: usize,
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
    /// Files the comparison hides in the document, left out of the tree.
    hidden: usize,
    /// Files the comparison would hide, shown anyway (dimmed in the document).
    hideable_shown: usize,
    /// Every file of the tree's sources, hidden ones included.
    total: usize,
    /// The sources the tree shows, in order.
    sources: Vec<String>,
    /// Tree or flat list.
    layout: FileLayout,
}

impl FileTree {
    /// The tree of every file in `doc`.
    #[must_use]
    pub fn new(doc: &ReviewDocument, theme: &ReviewTheme) -> Self {
        Self::with_layout(doc, theme, FileLayout::Tree)
    }

    /// Every file in `doc`, as a tree or a flat list.
    #[must_use]
    pub fn with_layout(doc: &ReviewDocument, theme: &ReviewTheme, layout: FileLayout) -> Self {
        let (groups, hideable_shown) = collect_groups(doc);
        let flat = match layout {
            FileLayout::Flat(flat) => Some(flat),
            FileLayout::Tree => None,
        };
        let merge = flat.is_some_and(|flat| flat.merge_groups);
        let grouped = groups.len() > 1 && !merge;
        let tag_sources = merge && groups.len() > 1;
        let mut tree = Self {
            hideable_shown,
            layout,
            ..Self::default()
        };
        // A merged flat list: every source's files, then every source's dropped files.
        let mut merged = Vec::new();
        let mut merged_gone = Vec::new();
        for GroupRows {
            source,
            part,
            mut rows,
            hidden,
        } in groups
        {
            tree.hidden += hidden;
            tree.total += hidden + rows.len();
            if !tree.sources.contains(&source) {
                tree.sources.push(source.clone());
            }
            let depth = if grouped {
                let label = group_label(doc, &source, part);
                let compare = group_compare(doc, &source, hidden);
                tree.push(
                    format!("g{SEP}{source}{SEP}{}", part_text(part)),
                    0,
                    None,
                    NodeKind::Group {
                        label,
                        totals: Totals::default(),
                        compare,
                    },
                );
                1
            } else {
                0
            };
            let parent = grouped.then(|| tree.nodes.len() - 1);
            let prefix = format!("{source}{SEP}{}{SEP}", part_text(part));
            match flat {
                None => tree.emit_tree(rows, depth, parent, &prefix),
                Some(flat) => {
                    if tag_sources {
                        let label = group_label(doc, &source, part);
                        for row in &mut rows {
                            row.source_tag = Some(label.clone());
                        }
                    }
                    if merge {
                        merged.extend(rows);
                    } else {
                        tree.emit_flat(rows, flat.order, depth, parent);
                    }
                }
            }
            // Files the baseline's change had and the current one dropped, once per source.
            if part.is_none_or(|part| part == 0) {
                for path in dropped_paths(doc, &source) {
                    let id = format!("x{SEP}{prefix}{path}");
                    if merge {
                        merged_gone.push((id, path));
                    } else {
                        tree.push(id, depth, parent, NodeKind::Gone { path });
                    }
                }
            }
        }
        if let Some(flat) = flat.filter(|_| merge) {
            tree.emit_flat(merged, flat.order, 0, None);
            for (id, path) in merged_gone {
                tree.push(id, 0, None, NodeKind::Gone { path });
            }
        }
        tree.aggregate();
        tree.item_of = vec![None; tree.nodes.len()];
        tree.rebuild_items(theme);
        tree
    }

    /// Emit `rows` (one source's files, in document order) as directories and files.
    fn emit_tree(&mut self, rows: Vec<FileRow>, depth: u16, parent: Option<usize>, prefix: &str) {
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
        self.emit(&mut dirs, 0, depth, parent, prefix, "");
    }

    /// Emit `rows` (in document order) as a flat list in `order`.
    fn emit_flat(
        &mut self,
        mut rows: Vec<FileRow>,
        order: FlatOrder,
        depth: u16,
        parent: Option<usize>,
    ) {
        if order == FlatOrder::Path {
            // Stable: one path in several sources keeps their document order.
            rows.sort_by(|left, right| left.file.path.cmp(&right.file.path));
        }
        for row in rows {
            let id = file_node_id(&row.file);
            self.push(id, depth, parent, NodeKind::File(row));
        }
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
                    let id = file_node_id(&row.file);
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
                            path: full.clone(),
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

    /// Ids of the group rows (one per source or part when there are several) with their source.
    pub fn group_ids(&self) -> impl Iterator<Item = (&str, &str)> {
        self.nodes.iter().filter_map(|node| match node.kind {
            NodeKind::Group { .. } => {
                let source = node.id.split(SEP).nth(1)?;
                Some((node.id.as_str(), source))
            }
            _ => None,
        })
    }

    /// Ids of every directory and group, for the initial (fully expanded) state.
    pub fn expandable_ids(&self) -> impl Iterator<Item = &str> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Group { .. } | NodeKind::Dir { .. }))
            .map(|node| node.id.as_str())
    }

    /// Files the comparison hides in the document, left out of the tree.
    #[must_use]
    pub const fn hidden_files(&self) -> usize {
        self.hidden
    }

    /// Files the comparison would hide, shown anyway.
    #[must_use]
    pub const fn hideable_shown(&self) -> usize {
        self.hideable_shown
    }

    /// Every file of the tree's sources, the hidden ones included.
    #[must_use]
    pub const fn total_files(&self) -> usize {
        self.total
    }

    /// The sources the tree shows, in order.
    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
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

    /// The repository-relative path of a directory row id.
    #[must_use]
    pub fn folder_path(&self, id: &str) -> Option<&str> {
        match &self.nodes[*self.ids.get(id)?].kind {
            NodeKind::Dir { path, .. } => Some(path),
            _ => None,
        }
    }

    /// Whether `id` is a directory or group row.
    #[must_use]
    pub fn is_expandable(&self, id: &str) -> bool {
        self.ids.get(id).is_some_and(|index| {
            matches!(
                self.nodes[*index].kind,
                NodeKind::Group { .. } | NodeKind::Dir { .. }
            )
        })
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

    /// Tree or flat list.
    #[must_use]
    pub const fn layout(&self) -> FileLayout {
        self.layout
    }

    /// Rebuild every displayed item (after a filter, the hide-viewed toggle, or a theme change).
    pub fn rebuild_items(&mut self, theme: &ReviewTheme) {
        let filtering = !self.query.is_empty();
        let mut keep = vec![false; self.nodes.len()];
        let mut matches = 0;
        for (index, node) in self.nodes.iter().enumerate() {
            let path = match &node.kind {
                NodeKind::File(row) => {
                    if self.hide_viewed && row.decorations.viewed {
                        continue;
                    }
                    &row.file.path
                }
                NodeKind::Gone { path } => path,
                // A compared group says what its comparison found even with no file shown.
                NodeKind::Group {
                    compare: Some(_), ..
                } if !filtering => {
                    keep[index] = true;
                    continue;
                }
                _ => continue,
            };
            let path = path.to_lowercase();
            if self.query.iter().all(|word| path.contains(word.as_str())) {
                if matches!(node.kind, NodeKind::File(_)) {
                    matches += 1;
                }
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
        let Some(decorations) = header(doc, file).map(|header| decorations_of(doc, header)) else {
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

    /// A file row: status, name, and decorations, marked by how it changed since the baseline.
    fn file_item(node: &Node, row: &FileRow, theme: &ReviewTheme, flat: bool) -> TreeViewItem {
        let icons = &theme.icons;
        let decorations = &row.decorations;
        let since = decorations.since;
        let name_style = if row.decorations.viewed {
            theme.path_viewed
        } else if since == Some(FileSince::Unchanged) {
            // The same as at the baseline: receded.
            theme.muted
        } else if row.status == ReviewFileStatus::Deleted {
            theme.text.add_modifier(Modifier::CROSSED_OUT)
        } else if since.is_some_and(FileSince::changed) {
            theme.text.add_modifier(Modifier::BOLD)
        } else {
            theme.text
        };
        let mut spans = Vec::new();
        // Changed and new files stand out with a mark before their status.
        match since {
            Some(FileSince::Changed { .. }) => {
                spans.push(Span::styled(icons.since_changed, theme.since.badge));
                spans.push(Span::raw(" "));
            }
            Some(FileSince::New { .. }) => {
                spans.push(Span::styled(icons.since_new_to_you, theme.since.badge));
                spans.push(Span::raw(" "));
            }
            _ => {}
        }
        let status_style = if since == Some(FileSince::Unchanged) {
            theme.muted
        } else {
            theme.status(row.status)
        };
        spans.push(Span::styled(status_letter(row.status), status_style));
        spans.push(Span::raw(" "));
        // Flat, a file reads by its whole path; its directories give way first when narrow.
        let mut shrink = None;
        if flat && let Some((directory, _)) = row.file.path.rsplit_once('/') {
            shrink = Some(spans.len());
            spans.push(Span::styled(format!("{directory}/"), theme.muted));
        }
        spans.push(Span::styled(row.name.clone(), name_style));
        let label = Line::from_spans(spans);
        let mut trailing = Vec::new();
        // Leftmost, so the first to go when the row is narrow.
        if let Some(tag) = &row.source_tag {
            trailing.push(Line::from_spans(vec![Span::styled(
                tag.clone(),
                theme.muted,
            )]));
        }
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
        if let Some(since) = since_badge(since, theme) {
            trailing.push(since);
        }
        trailing.push(Line::from_spans(vec![if decorations.viewed {
            Span::styled(icons.viewed, theme.viewed)
        } else {
            Span::styled(icons.unviewed, theme.muted)
        }]));
        let item = TreeViewItem::rich(node.id.clone(), label, node.depth).trailing(trailing);
        match shrink {
            Some(span) => item.shrink_span(span),
            None => item,
        }
    }

    fn item(&self, index: usize, theme: &ReviewTheme, filtering: bool) -> TreeViewItem {
        let node = &self.nodes[index];
        let icons = &theme.icons;
        match &node.kind {
            NodeKind::File(row) => Self::file_item(node, row, theme, self.layout.is_flat()),
            NodeKind::Gone { path } => TreeViewItem::rich(
                node.id.clone(),
                Line::from_spans(vec![
                    Span::styled(icons.minus, theme.muted),
                    Span::raw(" "),
                    Span::styled(
                        path.clone(),
                        theme.muted.add_modifier(Modifier::CROSSED_OUT),
                    ),
                ]),
                node.depth,
            )
            .trailing([Line::from_spans(vec![Span::styled(
                "dropped since",
                theme.muted,
            )])])
            .disabled(true),
            NodeKind::Dir { name, totals, .. }
            | NodeKind::Group {
                label: name,
                totals,
                ..
            } => {
                // Directories end with a slash; source groups are named, not paths, and say
                // what their comparison found.
                let label = if let NodeKind::Group { compare, .. } = &node.kind {
                    group_line(name, compare.as_ref(), theme)
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
                // A compared group's label says what changed since the baseline: its whole
                // change's counts give that room. With no file shown, it says nothing more.
                let compared = matches!(
                    node.kind,
                    NodeKind::Group {
                        compare: Some(_),
                        ..
                    }
                );
                if !compared {
                    trailing.push(Line::from_spans(count_spans(
                        totals.additions,
                        totals.deletions,
                        theme,
                    )));
                }
                if compared && totals.files == 0 {
                    return TreeViewItem::rich(node.id.clone(), label, node.depth)
                        .trailing(trailing)
                        .expandable(!filtering);
                }
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

/// A file row's id: the same in a tree and a flat list.
fn file_node_id(file: &FileRef) -> String {
    format!(
        "f{SEP}{}{SEP}{}{SEP}{}",
        file.source_id,
        part_text(file.part),
        file.path
    )
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
) -> Option<&'a bcode_code_review::document::FileHeaderItem> {
    let index = doc.index_of(&ItemKey::FileHeader(file.clone()))?;
    match doc.item(index)? {
        ReviewItem::FileHeader(header) => Some(header),
        _ => None,
    }
}

fn decorations_of(
    doc: &ReviewDocument,
    header: &bcode_code_review::document::FileHeaderItem,
) -> Decorations {
    Decorations {
        viewed: header.viewed,
        threads: u32::try_from(header.thread_count).unwrap_or(u32::MAX),
        // Draft counts arrive with the annotation pipeline.
        drafts: 0,
        layers: header.layers.clone(),
        since: doc.file_since(&header.file),
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
        source_tag: None,
        status: header.status,
        additions: u64::from(header.additions),
        deletions: u64::from(header.deletions),
        binary: header.flags.contains(&ReviewIndexFlag::Binary),
        decorations: decorations_of(doc, header),
    })
}

/// The files the tree shows, grouped by source and part (hidden ones counted on their group),
/// and how many files the comparison would hide are shown anyway.
fn collect_groups(doc: &ReviewDocument) -> (Vec<GroupRows>, usize) {
    let mut groups: Vec<GroupRows> = Vec::new();
    let mut hideable_shown = 0;
    let focus = doc.focus();
    for file in doc.file_keys() {
        // One layer at a time: only the focused source's files.
        if focus.is_some_and(|focus| focus != file.source_id)
            || doc.source_filtered(&file.source_id)
        {
            continue;
        }
        let same = groups.last().is_some_and(|group| {
            (group.source.as_str(), group.part) == (file.source_id.as_str(), file.part)
        });
        if !same {
            groups.push(GroupRows {
                source: file.source_id.clone(),
                part: file.part,
                rows: Vec::new(),
                hidden: 0,
            });
        }
        let Some(group) = groups.last_mut() else {
            continue;
        };
        // Left out here as in the document, and counted on the group.
        if doc.file_compare_hidden(file) {
            group.hidden += 1;
            continue;
        }
        if doc.file_compare_hideable(file) {
            hideable_shown += 1;
        }
        if let Some(row) = file_row(doc, file) {
            group.rows.push(row);
        }
    }
    (groups, hideable_shown)
}

/// Label of a source group row: its header label, or a stack layer's pull request.
fn group_label(doc: &ReviewDocument, source: &str, part: Option<u32>) -> String {
    let key = ItemKey::SourceHeader {
        source_id: source.to_owned(),
        part,
    };
    doc.index_of(&key)
        .and_then(|index| match doc.item(index) {
            // A stack layer is named by its pull request, the way its banner and tab name it.
            Some(ReviewItem::SourceHeader(header)) => Some(match &header.layer {
                Some(layer) if !layer.whole_stack => format!("#{} {}", layer.number, layer.title),
                _ => header.label.clone(),
            }),
            _ => None,
        })
        .unwrap_or_else(|| source.to_owned())
}

/// What source `source`'s comparison found, when it is compared.
fn group_compare(doc: &ReviewDocument, source: &str, hidden: usize) -> Option<GroupCompare> {
    let comparison = doc.comparison(source)?;
    let summary = doc.comparison_summary(source).unwrap_or_default();
    Some(GroupCompare {
        kind: comparison.delta.as_ref().map(|delta| delta.kind),
        added: summary.added,
        removed: summary.removed,
        changed_files: summary.files,
        hidden,
    })
}

/// Paths source `source`'s baseline change had that its current change dropped.
fn dropped_paths(doc: &ReviewDocument, source: &str) -> Vec<String> {
    doc.comparison(source)
        .and_then(|comparison| comparison.delta.as_ref())
        .map(|delta| {
            delta
                .files
                .iter()
                .filter(|file| file.since == FileSince::Dropped)
                .map(|file| file.path.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// A group row's label: its name, then what its comparison found (`● +40 −12 · 3 files`,
/// `○ rebased only · 4 hidden`). Groups with nothing to look at again recede.
fn group_line(name: &str, compare: Option<&GroupCompare>, theme: &ReviewTheme) -> Line {
    let icons = &theme.icons;
    let Some(compare) = compare else {
        return Line::from_spans(vec![Span::styled(name.to_owned(), theme.source_header)]);
    };
    let quiet = matches!(
        compare.kind,
        Some(DeltaKind::Unchanged | DeltaKind::RebasedOnly)
    );
    let mut spans = vec![Span::styled(
        name.to_owned(),
        if quiet {
            theme.muted
        } else {
            theme.source_header
        },
    )];
    let (badge, style) = match compare.kind {
        None => (format!("{} comparing", icons.ellipsis), theme.muted),
        Some(kind @ (DeltaKind::Changed | DeltaKind::WithRebase)) => {
            let glyph = if kind == DeltaKind::WithRebase {
                icons.since_with_rebase
            } else {
                icons.since_changed
            };
            let files = compare.changed_files;
            (
                format!(
                    "{glyph} +{} {}{} · {files} file{}",
                    compare.added,
                    icons.minus,
                    compare.removed,
                    if files == 1 { "" } else { "s" }
                ),
                theme.since.badge,
            )
        }
        Some(DeltaKind::RebasedOnly) => {
            (format!("{} rebased only", icons.since_rebased), theme.muted)
        }
        Some(DeltaKind::Unchanged) => {
            (format!("{} no changes", icons.since_unchanged), theme.muted)
        }
        Some(DeltaKind::NewToYou) => (
            format!("{} new to you", icons.since_new_to_you),
            theme.accent,
        ),
        Some(DeltaKind::Unavailable) => (
            format!("{} baseline gone", icons.since_unavailable),
            theme.notice,
        ),
    };
    spans.push(Span::raw("  "));
    spans.push(Span::styled(badge, style));
    if compare.hidden > 0 {
        spans.push(Span::styled(
            format!(" · {} hidden", compare.hidden),
            theme.muted,
        ));
    }
    Line::from_spans(spans)
}

/// A file row's since counts (`+3 −1`, `new +12`), when it changed since the baseline.
fn since_badge(since: Option<FileSince>, theme: &ReviewTheme) -> Option<Line> {
    let minus = theme.icons.minus;
    let text = match since? {
        FileSince::Changed { added, removed } => {
            let delta = if theme.icons.menu.is_ascii() {
                "d"
            } else {
                "Δ"
            };
            format!("{delta}+{added}{minus}{removed}")
        }
        FileSince::New { added, .. } => format!("new +{added}"),
        _ => return None,
    };
    Some(Line::from_spans(vec![Span::styled(
        text,
        theme.since.badge,
    )]))
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
    use crate::testing::harness::paint;
    use crate::theme::IconStyle;
    use bcode_code_review::document::DocumentConfig;
    use bcode_code_review::testing::document::{Fake, SOURCE, SynthFile};
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

    fn delta(
        kind: DeltaKind,
        files: Vec<(&str, FileSince)>,
    ) -> bcode_code_review_models::LayerDelta {
        use bcode_code_review_models::{DeltaFile, LayerDelta, LayerPair, VersionPoint};
        let mut files: Vec<DeltaFile> = files
            .into_iter()
            .map(|(path, since)| DeltaFile {
                path: path.to_owned(),
                since,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        LayerDelta {
            pair: LayerPair {
                number: 7,
                from: Some(VersionPoint {
                    head_oid: "old".into(),
                    fork_oid: None,
                }),
                to: VersionPoint {
                    head_oid: "head".into(),
                    fork_oid: None,
                },
            },
            kind,
            replay_oid: Some("replay".into()),
            files,
        }
    }

    fn compare(
        doc: &mut ReviewDocument,
        source: &str,
        delta: bcode_code_review_models::LayerDelta,
        view: bcode_code_review::document::CompareView,
    ) {
        doc.set_comparison(
            source,
            Some(bcode_code_review::document::GroupComparison {
                delta: Some(delta),
                view,
                ..Default::default()
            }),
        );
    }

    fn compared() -> Fake {
        Fake::new(vec![
            SynthFile::added("src/changed.rs", 10),
            SynthFile::added("src/new.rs", 4),
            SynthFile::added("src/same.rs", 6),
        ])
    }

    fn changed_delta() -> bcode_code_review_models::LayerDelta {
        delta(
            DeltaKind::Changed,
            vec![
                (
                    "src/changed.rs",
                    FileSince::Changed {
                        added: 3,
                        removed: 1,
                    },
                ),
                (
                    "src/new.rs",
                    FileSince::New {
                        added: 4,
                        removed: 0,
                    },
                ),
                ("src/gone.rs", FileSince::Dropped),
            ],
        )
    }

    fn label_of<'a>(tree: &'a FileTree, needle: &str) -> &'a TreeViewItem {
        tree.items()
            .iter()
            .find(|item| item.label.contains(needle))
            .unwrap_or_else(|| panic!("no row {needle}: {:?}", tree.items()))
    }

    #[test]
    fn compared_files_are_marked_receded_and_dropped_ones_listed() {
        use bcode_code_review::document::CompareView;
        let fake = compared();
        let mut doc = fake.document(DocumentConfig::default());
        compare(&mut doc, SOURCE, changed_delta(), CompareView::Full);
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        // Changed and new files are marked before their status, with their since counts.
        let changed = label_of(&tree, "changed.rs");
        assert!(changed.label.starts_with("● "), "{:?}", changed.label);
        let trailing: Vec<String> = changed.trailing.iter().map(Line::plain_text).collect();
        assert!(trailing.contains(&"Δ+3−1".to_owned()), "{trailing:?}");
        assert!(label_of(&tree, "new.rs").label.starts_with("+ "));
        // The unchanged file is listed, unmarked, in the full view.
        let same = label_of(&tree, "same.rs");
        assert!(same.label.starts_with("A "), "{:?}", same.label);
        // The dropped file is listed, struck through and inert.
        let gone = label_of(&tree, "src/gone.rs");
        assert!(gone.disabled);
        assert!(tree.file_of(&gone.id).is_none());
        assert!(!tree.is_expandable(&gone.id));
        assert_eq!(tree.hidden_files(), 0);
        assert_eq!(tree.file_count(), 3);
    }

    #[test]
    fn the_changes_view_leaves_out_unchanged_files_like_the_document() {
        use bcode_code_review::document::CompareView;
        let fake = compared();
        let mut doc = fake.document(DocumentConfig::default());
        compare(&mut doc, SOURCE, changed_delta(), CompareView::Changes);
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        assert!(
            !tree
                .items()
                .iter()
                .any(|item| item.label.contains("same.rs"))
        );
        assert_eq!(tree.hidden_files(), 1);
        assert_eq!(tree.total_files(), 3);
        // Shown anyway: back in the tree, counted as shown.
        doc.set_compare_files_shown(SOURCE, true);
        let tree = FileTree::new(&doc, &theme);
        assert!(
            tree.items()
                .iter()
                .any(|item| item.label.contains("same.rs"))
        );
        assert_eq!(tree.hidden_files(), 0);
        assert_eq!(tree.hideable_shown(), 1);
    }

    #[test]
    fn group_rows_say_what_each_comparison_found_and_stay_when_all_is_hidden() {
        use bcode_code_review::document::CompareView;
        let fake = compared();
        let mut doc = ReviewDocument::new(
            std::path::PathBuf::from("/repo"),
            vec![fake.input("a"), fake.input("b"), fake.input("c")],
            DocumentConfig::default(),
        );
        compare(&mut doc, "a", changed_delta(), CompareView::Full);
        compare(
            &mut doc,
            "b",
            delta(DeltaKind::RebasedOnly, Vec::new()),
            CompareView::Full,
        );
        doc.set_comparison(
            "c",
            Some(bcode_code_review::document::GroupComparison::default()),
        );
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        let groups: Vec<&str> = tree
            .items()
            .iter()
            .filter(|item| item.depth == 0 && item.id.starts_with('g'))
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(groups.len(), 3, "{groups:?}");
        assert!(groups[0].ends_with("● +7 −1 · 3 files"), "{groups:?}");
        // Only rebased: every file hidden, the group still says so.
        assert!(
            groups[1].ends_with("○ rebased only · 3 hidden"),
            "{groups:?}"
        );
        assert!(groups[2].ends_with("⋯ comparing"), "{groups:?}");
        assert_eq!(tree.hidden_files(), 3);
        assert_eq!(tree.sources(), ["a", "b", "c"]);
    }

    #[test]
    fn compared_rows_stay_inside_tiny_widths() {
        use bcode_code_review::document::CompareView;
        let fake = compared();
        let mut doc = ReviewDocument::new(
            std::path::PathBuf::from("/repo"),
            vec![fake.input("漢字ソース"), fake.input("b")],
            DocumentConfig::default(),
        );
        compare(&mut doc, "漢字ソース", changed_delta(), CompareView::Full);
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
        let height = u16::try_from(tree.items().len()).unwrap();
        for width in [0_u16, 1, 2, 4, 9, 18, 30, 60] {
            let screen = paint(width + 2, height, |area, cx| {
                let layout = component.layout(
                    Constraints::tight(Size::new(width, area.height)),
                    &mut LayoutCx::new(),
                );
                cx.with_child(1, 0, LocalRect::new(0, 0, width, area.height), |cx| {
                    component.paint(&layout, cx);
                });
            });
            for y in 0..height {
                assert_eq!(screen.cell(0, y).unwrap().symbol, " ", "{width}: {y}");
                assert_eq!(
                    screen.cell(width + 1, y).unwrap().symbol,
                    " ",
                    "{width}: {y}"
                );
                assert!(!screen.row_text(y).contains('\u{fffd}'));
            }
            for hit in screen.hits() {
                assert!(
                    hit.area.x >= 1 && hit.area.right() <= width + 1,
                    "{width}: {hit:?}"
                );
            }
        }
    }

    fn flat(order: FlatOrder, merge_groups: bool) -> FileLayout {
        FileLayout::Flat(FlatLayout {
            order,
            merge_groups,
        })
    }

    #[test]
    fn a_flat_list_shows_whole_paths_with_the_tree_ids() {
        let doc = sample().document(DocumentConfig::default());
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let tree = FileTree::new(&doc, &theme);
        let list = FileTree::with_layout(&doc, &theme, flat(FlatOrder::Document, false));
        assert!(list.layout().is_flat());
        assert_eq!(list.file_count(), tree.file_count());
        // No directories: every row is a file at the top level.
        assert!(
            list.items()
                .iter()
                .all(|item| item.depth == 0 && !item.expandable)
        );
        assert_eq!(list.items().len(), 7);
        let deep = list
            .items()
            .iter()
            .find(|item| item.label.ends_with("deep.rs"))
            .unwrap();
        assert_eq!(deep.label, "A a/b/c/deep.rs");
        // The directories give way first when the row is narrow; the name stays.
        let span = deep.shrink_span.expect("a directory span");
        let narrow = deep
            .rich_label
            .as_ref()
            .unwrap()
            .truncate_shrinking(12, span);
        assert!(narrow.plain_text().ends_with("deep.rs"), "{narrow:?}");
        let top = list
            .items()
            .iter()
            .find(|item| item.label.ends_with("top.rs"))
            .unwrap();
        assert_eq!((top.label.as_str(), top.shrink_span), ("A top.rs", None));
        // Ids are the tree's: selection carries across a switch.
        for file in doc.file_keys() {
            assert_eq!(list.file_id(file), tree.file_id(file));
        }
        // The filter matches whole paths, as in the tree.
        let mut list = list;
        assert!(list.set_filter("DEEP", &theme));
        let labels: Vec<&str> = list
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["A a/b/c/deep.rs"]);
    }

    #[test]
    fn a_flat_list_keeps_or_merges_groups_in_document_or_path_order() {
        let fake = Fake::new(vec![
            SynthFile::added("a/two.rs", 2),
            SynthFile::added("z/one.rs", 1),
        ]);
        let doc = ReviewDocument::new(
            std::path::PathBuf::from("/repo"),
            vec![fake.input("first"), fake.input("second")],
            DocumentConfig::default(),
        );
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let rows = |tree: &FileTree| -> Vec<(u16, String, Option<String>)> {
            tree.items()
                .iter()
                .map(|item| {
                    let tag = item
                        .trailing
                        .first()
                        .map(Line::plain_text)
                        .filter(|tag| tag == "first" || tag == "second");
                    (item.depth, item.label.clone(), tag)
                })
                .collect()
        };
        // Kept groups: each source's files under its group row.
        let kept = FileTree::with_layout(&doc, &theme, flat(FlatOrder::Document, false));
        let kept_rows = rows(&kept);
        assert_eq!(kept_rows.len(), 6, "{kept_rows:?}");
        assert_eq!(kept_rows[0].0, 0);
        assert_eq!(
            kept_rows[1..3]
                .iter()
                .map(|row| (row.0, row.1.as_str()))
                .collect::<Vec<_>>(),
            [(1, "A a/two.rs"), (1, "A z/one.rs")]
        );
        assert_eq!(kept.group_ids().count(), 2);
        // Merged, in document order: one source after the other, each row naming its source.
        let merged = FileTree::with_layout(&doc, &theme, flat(FlatOrder::Document, true));
        let document_order = |tree: &FileTree| -> Vec<(String, Option<String>)> {
            rows(tree)
                .into_iter()
                .map(|(depth, label, tag)| {
                    assert_eq!(depth, 0);
                    (label, tag)
                })
                .collect()
        };
        let tagged = |label: &str, tag: &str| (label.to_owned(), Some(tag.to_owned()));
        assert_eq!(
            document_order(&merged),
            [
                tagged("A a/two.rs", "first"),
                tagged("A z/one.rs", "first"),
                tagged("A a/two.rs", "second"),
                tagged("A z/one.rs", "second"),
            ]
        );
        assert_eq!(merged.group_ids().count(), 0);
        // Merged, by path: one path's rows together, in document order among themselves.
        let by_path = FileTree::with_layout(&doc, &theme, flat(FlatOrder::Path, true));
        assert_eq!(
            document_order(&by_path),
            [
                tagged("A a/two.rs", "first"),
                tagged("A a/two.rs", "second"),
                tagged("A z/one.rs", "first"),
                tagged("A z/one.rs", "second"),
            ]
        );
    }
}
