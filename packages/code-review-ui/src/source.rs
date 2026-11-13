//! Lazy BMUX item source over the review document.
//!
//! Item keys are the document's stable keys, so list positions survive loads, evictions,
//! expansion, and layout switches; membership changes reach the list as the document's splices
//! (noted by the surface), never by rescanning. File headers (level 0) and hunk headers
//! (level 1) are sticky sections, with parents answered from the document's section index.

use super::rows::{Interaction, ItemFlags, ItemView, RowContext, RowKey};
use bcode_code_review::document::{ItemKey, ReviewItem};
use bmux_tui::component::{Element, LayoutId};
use bmux_tui_components::virtual_list::{PinnedPlacement, VirtualItemSource};

/// Node id of every row component.
const ROW_ID: &str = "review.row";

/// Paint-only decoration of items: the cursor, the selected range, and the hovered item.
#[derive(Debug, Clone, Copy, Default)]
pub struct Decor<'a> {
    /// Cursor item.
    pub cursor: Option<&'a ItemKey>,
    /// Inclusive item index range of the selected lines.
    pub selected: Option<(usize, usize)>,
    /// Item under the pointer.
    pub hovered: Option<&'a ItemKey>,
    /// Revision of search highlights.
    pub search_revision: u64,
}

/// Item source of the review list.
pub struct DocumentSource<'a> {
    context: &'a RowContext<'a>,
    interaction: &'a Interaction,
    decor: Decor<'a>,
}

impl<'a> DocumentSource<'a> {
    /// Source over `context`, decorated by `decor`; clicks and pointer events go to
    /// `interaction`.
    #[must_use]
    pub const fn new(
        context: &'a RowContext<'a>,
        interaction: &'a Interaction,
        decor: Decor<'a>,
    ) -> Self {
        Self {
            context,
            interaction,
            decor,
        }
    }

    fn flags(&self, index: usize, key: &ItemKey) -> ItemFlags {
        ItemFlags {
            cursor: self.decor.cursor == Some(key),
            selected: self
                .decor
                .selected
                .is_some_and(|(first, last)| (first..=last).contains(&index)),
            hovered: self.decor.hovered == Some(key),
            pinned_edge: false,
        }
    }
}

impl VirtualItemSource<RowKey> for DocumentSource<'_> {
    fn len(&self) -> usize {
        self.context.doc.len()
    }

    fn key(&self, index: usize) -> RowKey {
        RowKey(
            self.context
                .doc
                .item(index)
                .map_or(ItemKey::Composer, ReviewItem::key),
        )
    }

    fn layout_revision(&self, index: usize) -> u64 {
        // Content changes arrive as splices (new items); only presentation-wide changes
        // re-measure items in place, plus annotation content and the live composer.
        let base = self.context.params.layout_revision;
        match self.context.doc.item(index) {
            Some(
                item @ (ReviewItem::Thread(_)
                | ReviewItem::AgentThread(_)
                | ReviewItem::SourceIntro(_)),
            ) => {
                base ^ self
                    .context
                    .annotation_stamp(item)
                    .wrapping_mul(0x9e37_79b9)
            }
            Some(ReviewItem::Composer(_)) => {
                base ^ self
                    .context
                    .composer
                    .map_or(0, super::composer::Composer::revision)
                    .wrapping_mul(0x85eb_ca6b)
            }
            _ => base,
        }
    }

    fn paint_revision(&self, index: usize) -> u64 {
        let flags = self
            .context
            .doc
            .item(index)
            .map_or(0, |item| self.flags(index, &item.key()).bits());
        self.context
            .params
            .paint_revision
            .wrapping_mul(31)
            .wrapping_add(self.context.syntax.epoch())
            .wrapping_mul(31)
            .wrapping_add(self.decor.search_revision)
            .wrapping_mul(8)
            .wrapping_add(flags)
    }

    fn build(&self, index: usize) -> Element<'_> {
        let key = self
            .context
            .doc
            .item(index)
            .map_or(ItemKey::Composer, ReviewItem::key);
        // The list identifies items by their keys (wrapper ids); the row's own node id is not
        // used for lookups, so one static id avoids formatting a key per built item.
        let id = LayoutId::new(ROW_ID);
        let flags = self.flags(index, &key);
        Element::new(ItemView::new(
            id,
            key,
            index,
            self.context,
            self.interaction,
            flags,
        ))
    }

    fn measure_hint(
        &self,
        index: usize,
        width: u64,
        _environment: bmux_tui::component::LayoutEnvironment,
    ) -> Option<u64> {
        self.context
            .height(index, u16::try_from(width).unwrap_or(u16::MAX))
    }

    fn build_pinned(&self, index: usize, placement: PinnedPlacement) -> Element<'_> {
        let key = self
            .context
            .doc
            .item(index)
            .map_or(ItemKey::Composer, ReviewItem::key);
        let mut flags = self.flags(index, &key);
        flags.pinned_edge = placement.deepest;
        Element::new(ItemView::new(
            LayoutId::new(ROW_ID),
            key,
            index,
            self.context,
            self.interaction,
            flags,
        ))
    }

    fn section_header_level(&self, index: usize) -> Option<u16> {
        self.context
            .doc
            .item(index)
            .and_then(ReviewItem::header_level)
            .map(u16::from)
    }

    fn section_parent(&self, index: usize) -> Option<usize> {
        let sections = self.context.doc.sections(index);
        match self.context.doc.item(index)?.header_level() {
            Some(0) => None,
            Some(_) => sections.file_header,
            None => sections.hunk_header.or(sections.file_header),
        }
    }
}
