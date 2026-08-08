//! Retained Markdown projections for transcript entries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use bcode_markdown_render::{MarkdownRenderOptions, MarkdownRenderResult, render_markdown};

use super::transcript::TranscriptItem;

const MAX_PRESENTATION_VARIANTS_PER_ITEM: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedMarkdownProjection {
    item_revision: u64,
    options: MarkdownRenderOptions,
    result: Arc<MarkdownRenderResult>,
}

#[derive(Debug, Clone, Default)]
struct CachedMarkdownProjections {
    current: Option<CachedMarkdownProjection>,
    variants: Vec<CachedMarkdownProjection>,
}

impl CachedMarkdownProjections {
    #[cfg(test)]
    fn len(&self) -> usize {
        usize::from(self.current.is_some()).saturating_add(self.variants.len())
    }

    fn get(
        &self,
        item_revision: u64,
        options: &MarkdownRenderOptions,
    ) -> Option<Arc<MarkdownRenderResult>> {
        self.current
            .iter()
            .chain(&self.variants)
            .find(|cached| cached.item_revision == item_revision && &cached.options == options)
            .map(|cached| Arc::clone(&cached.result))
    }

    fn get_previous_compatible(
        &self,
        options: &MarkdownRenderOptions,
    ) -> Option<Arc<MarkdownRenderResult>> {
        self.current
            .iter()
            .chain(&self.variants)
            .find(|cached| {
                let mut cached_options = cached.options.clone();
                cached_options.streaming = options.streaming;
                &cached_options == options
            })
            .map(|cached| Arc::clone(&cached.result))
    }

    fn install(&mut self, projection: CachedMarkdownProjection) {
        if let Some(current) = self.current.replace(projection) {
            self.variants.retain(|cached| {
                cached.item_revision != current.item_revision || cached.options != current.options
            });
            self.variants.push(current);
            if self.variants.len() > MAX_PRESENTATION_VARIANTS_PER_ITEM {
                let overflow = self
                    .variants
                    .len()
                    .saturating_sub(MAX_PRESENTATION_VARIANTS_PER_ITEM);
                self.variants.drain(..overflow);
            }
        }
    }
}

/// Per-transcript-item Markdown render cache.
///
/// Per resident item, retain the current projection plus a bounded set of recent presentation
/// variants so cycling among themes reuses parsed and projected Markdown.
#[derive(Debug, Default)]
pub struct TranscriptMarkdownCache {
    entries: RwLock<BTreeMap<u64, CachedMarkdownProjections>>,
    retained_revision: std::sync::atomic::AtomicU64,
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
            retained_revision: std::sync::atomic::AtomicU64::new(
                self.retained_revision
                    .load(std::sync::atomic::Ordering::Relaxed),
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
        let cached = {
            self.entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&item_id)
                .and_then(|cached| cached.get(item.revision(), &options))
        };
        if let Some(cached) = cached {
            return cached;
        }

        let result = Arc::new(render_markdown(item.text(), &options));
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(item_id)
            .or_default()
            .install(CachedMarkdownProjection {
                item_revision: item.revision(),
                options,
                result: Arc::clone(&result),
            });
        #[cfg(test)]
        self.render_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        result
    }

    /// Install a completed projection after its generation was accepted.
    pub fn install(
        &self,
        item_id: u64,
        item_revision: u64,
        options: MarkdownRenderOptions,
        result: Arc<MarkdownRenderResult>,
    ) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(item_id)
            .or_default()
            .install(CachedMarkdownProjection {
                item_revision,
                options,
                result,
            });
    }

    /// Return the retained projection only when the exact generation is cached.
    #[must_use]
    pub fn get(
        &self,
        item_id: u64,
        item_revision: u64,
        options: &MarkdownRenderOptions,
    ) -> Option<Arc<MarkdownRenderResult>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&item_id)
            .and_then(|cached| cached.get(item_revision, options))
    }

    /// Return a retained older revision with compatible render options.
    ///
    /// Streaming finalization alone may reuse the previous accepted projection while the exact
    /// terminal generation is in flight. Width and every other layout-affecting option must match.
    #[must_use]
    pub fn get_previous_compatible(
        &self,
        item_id: u64,
        options: &MarkdownRenderOptions,
    ) -> Option<Arc<MarkdownRenderResult>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&item_id)
            .and_then(|cached| cached.get_previous_compatible(options))
    }

    /// Return whether the current exact generation is already retained.
    #[must_use]
    pub fn contains(
        &self,
        item_id: u64,
        item_revision: u64,
        options: &MarkdownRenderOptions,
    ) -> bool {
        self.get(item_id, item_revision, options).is_some()
    }

    /// Remove stale projections once after the transcript document changes.
    pub fn retain_resident_iter<'a>(
        &self,
        resident_items: impl Iterator<Item = &'a TranscriptItem>,
        transcript_revision: u64,
    ) {
        if self
            .retained_revision
            .load(std::sync::atomic::Ordering::Relaxed)
            == transcript_revision
        {
            return;
        }
        let resident_ids = resident_items
            .map(|item| item.id().get())
            .collect::<BTreeSet<_>>();
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|item_id, _cached| resident_ids.contains(item_id));
        self.retained_revision
            .store(transcript_revision, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    fn retained_projection_count(&self, item_id: u64) -> usize {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&item_id)
            .map_or(0, CachedMarkdownProjections::len)
    }

    /// Return the number of actual Markdown renders performed by this cache.
    #[cfg(test)]
    pub fn render_count(&self) -> usize {
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
    fn theme_and_syntax_palette_changes_replace_projection_exactly_once() {
        use bcode_syntax_render::{SyntaxColor, SyntaxPalette};
        use bmux_tui::style::{Color, Style};

        let cache = TranscriptMarkdownCache::default();
        let item = TranscriptItem::with_format(
            "Bcode",
            "# Heading\n\n```rust\nfn main() {}\n```".to_owned(),
            TextFormat::Markdown,
        );
        let document_id = format!("transcript:{}", item.id().get());
        let base = MarkdownRenderOptions::new(80).with_document_id(document_id);
        let first = cache.project(&item, base.clone());

        let mut themed = base.clone();
        themed.theme.heading = Style::new().fg(Color::Magenta);
        let themed_result = cache.project(&item, themed.clone());
        let themed_repeat = cache.project(&item, themed);
        assert!(!Arc::ptr_eq(&first, &themed_result));
        assert!(Arc::ptr_eq(&themed_result, &themed_repeat));

        let color = |value| SyntaxColor::rgb(value, value, value);
        let palette = SyntaxPalette {
            text: color(1),
            comment: color(2),
            keyword: color(3),
            function: color(4),
            variable: color(5),
            string: color(6),
            number: color(7),
            type_name: color(8),
            operator: color(9),
            punctuation: color(10),
        };
        let palette_options = base.clone().with_syntax_palette(palette);
        let palette_result = cache.project(&item, palette_options.clone());
        let palette_repeat = cache.project(&item, palette_options);
        assert!(!Arc::ptr_eq(&themed_result, &palette_result));
        assert!(Arc::ptr_eq(&palette_result, &palette_repeat));
        assert_eq!(cache.render_count(), 3);
        let original_again = cache.project(&item, base);
        assert!(Arc::ptr_eq(&first, &original_again));
        assert_eq!(cache.render_count(), 3);
        assert_eq!(
            cache
                .entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[test]
    fn theme_variants_remain_strictly_bounded() {
        use bmux_tui::style::{Color, Style};

        let cache = TranscriptMarkdownCache::default();
        let item =
            TranscriptItem::with_format("Bcode", "# Heading".to_owned(), TextFormat::Markdown);
        let base = MarkdownRenderOptions::new(80)
            .with_document_id(format!("transcript:{}", item.id().get()));
        for index in 0..16_u8 {
            let mut options = base.clone();
            options.theme.heading = Style::new().fg(Color::Rgb(index, index, index));
            let _ = cache.project(&item, options);
        }

        assert_eq!(
            cache.retained_projection_count(item.id().get()),
            MAX_PRESENTATION_VARIANTS_PER_ITEM.saturating_add(1)
        );
    }

    #[test]
    fn layout_affecting_option_change_replaces_projection_once() {
        let cache = TranscriptMarkdownCache::default();
        let item = TranscriptItem::with_format(
            "Bcode",
            "<details><summary>More</summary>Body</details>".to_owned(),
            TextFormat::Markdown,
        );
        let base = MarkdownRenderOptions::new(80)
            .with_document_id(format!("transcript:{}", item.id().get()))
            .with_streaming(true);
        let first = cache.project(&item, base.clone());
        let second = cache.project(&item, base.with_streaming(false));
        let repeated = cache.project(
            &item,
            MarkdownRenderOptions::new(80)
                .with_document_id(format!("transcript:{}", item.id().get())),
        );

        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&second, &repeated));
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

    #[test]
    fn nonresident_projection_is_evicted() {
        let cache = TranscriptMarkdownCache::default();
        let item = TranscriptItem::with_format(
            "Bcode",
            "# Resident projection".to_owned(),
            TextFormat::Markdown,
        );
        cache.project(&item, MarkdownRenderOptions::new(80));
        cache.retain_resident_iter(std::slice::from_ref(&item).iter(), 1);
        assert_eq!(
            cache
                .entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        cache.retain_resident_iter(std::iter::empty(), 2);
        assert!(
            cache
                .entries
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn streaming_revision_retains_previous_projection_until_replacement() {
        let cache = TranscriptMarkdownCache::default();
        let mut item = TranscriptItem::with_identity(
            "Bcode",
            "first accepted revision".to_owned(),
            true,
            TextFormat::Markdown,
            super::super::transcript::TranscriptItemKind::AssistantMessage,
        );
        let options = MarkdownRenderOptions::new(80).with_streaming(true);
        let first = cache.project(&item, options.clone());
        item.append_text(" and newer content");

        cache.retain_resident_iter(std::slice::from_ref(&item).iter(), 1);

        let previous = cache
            .get_previous_compatible(item.id().get(), &options)
            .expect("previous streaming projection remains resident");
        assert!(Arc::ptr_eq(&first, &previous));
        assert!(
            cache
                .get(item.id().get(), item.revision(), &options)
                .is_none()
        );
    }

    #[test]
    fn finalization_retains_previous_streaming_projection_until_terminal_replacement() {
        let cache = TranscriptMarkdownCache::default();
        let item_id = 42;
        let streaming = MarkdownRenderOptions::new(80).with_streaming(true);
        let terminal = MarkdownRenderOptions::new(80).with_streaming(false);
        let accepted = Arc::new(render_markdown("accepted stream", &streaming));
        cache.install(item_id, 3, streaming, Arc::clone(&accepted));

        let previous = cache
            .get_previous_compatible(item_id, &terminal)
            .expect("streaming projection remains readable during finalization");

        assert!(Arc::ptr_eq(&accepted, &previous));
    }

    #[test]
    fn incompatible_width_does_not_reuse_previous_projection() {
        let cache = TranscriptMarkdownCache::default();
        let item_id = 42;
        let wide = MarkdownRenderOptions::new(80).with_streaming(true);
        let narrow = MarkdownRenderOptions::new(20).with_streaming(true);
        cache.install(
            item_id,
            3,
            wide.clone(),
            Arc::new(render_markdown("accepted stream", &wide)),
        );

        assert!(cache.get_previous_compatible(item_id, &narrow).is_none());
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
