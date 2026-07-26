//! Retained Markdown projections for transcript entries.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use bcode_markdown_render::{MarkdownRenderOptions, MarkdownRenderResult, render_markdown};

use super::transcript::TranscriptItem;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedMarkdownProjection {
    item_revision: u64,
    options: MarkdownRenderOptions,
    result: Arc<MarkdownRenderResult>,
}

/// Per-transcript-item Markdown render cache.
///
/// Only the current projection for each resident item is retained. Scrolling can
/// therefore reuse parsed Markdown and laid-out contribution geometry without
/// growing the cache for streaming revisions or terminal resizes.
#[derive(Debug, Default)]
pub struct TranscriptMarkdownCache {
    entries: RwLock<BTreeMap<u64, CachedMarkdownProjection>>,
    #[cfg(test)]
    render_count: std::sync::atomic::AtomicUsize,
}

impl Clone for TranscriptMarkdownCache {
    fn clone(&self) -> Self {
        Self {
            entries: RwLock::new(
                self.entries
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            #[cfg(test)]
            render_count: std::sync::atomic::AtomicUsize::new(self.render_count()),
        }
    }
}

impl TranscriptMarkdownCache {
    /// Return the current projection, rendering it once when its inputs changed.
    pub fn project(
        &self,
        item: &TranscriptItem,
        options: MarkdownRenderOptions,
    ) -> Arc<MarkdownRenderResult> {
        let item_id = item.id().get();
        if let Some(cached) = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&item_id)
            && cached.item_revision == item.revision()
            && cached.options == options
        {
            return Arc::clone(&cached.result);
        }

        let result = Arc::new(render_markdown(item.text(), &options));
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                item_id,
                CachedMarkdownProjection {
                    item_revision: item.revision(),
                    options,
                    result: Arc::clone(&result),
                },
            );
        #[cfg(test)]
        self.render_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        result
    }

    /// Remove projections whose transcript items are no longer resident.
    pub fn retain_resident(&self, resident_item_ids: &std::collections::BTreeSet<u64>) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|item_id, _| resident_item_ids.contains(item_id));
    }

    #[cfg(test)]
    fn render_count(&self) -> usize {
        self.render_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_view_models::TextFormat;

    #[test]
    fn unchanged_projection_is_rendered_once() {
        let cache = TranscriptMarkdownCache::default();
        let item = TranscriptItem::with_format(
            "Bcode",
            "[guide](https://example.com)".to_owned(),
            TextFormat::Markdown,
        );
        let options = MarkdownRenderOptions::new(80)
            .with_document_id(format!("transcript:{}", item.id().get()));

        let first = cache.project(&item, options.clone());
        let second = cache.project(&item, options);

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.render_count(), 1);
    }

    #[test]
    fn width_change_replaces_instead_of_accumulating_projection() {
        let cache = TranscriptMarkdownCache::default();
        let item = TranscriptItem::with_format(
            "Bcode",
            "one two three four five".to_owned(),
            TextFormat::Markdown,
        );

        cache.project(&item, MarkdownRenderOptions::new(80));
        cache.project(&item, MarkdownRenderOptions::new(10));

        assert_eq!(cache.render_count(), 2);
        assert_eq!(
            cache
                .entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }
}
