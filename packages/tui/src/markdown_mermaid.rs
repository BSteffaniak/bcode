//! Stable Mermaid diagram presentation lifecycle for the transcript TUI.

use std::collections::{BTreeMap, BTreeSet};

use bcode_mermaid_render::{
    MermaidCancellationToken, MermaidRenderCache, MermaidRenderError, MermaidRendered,
    MermaidRenderedOutput,
};
use bmux_tui::geometry::Rect;

/// Fixed rows reserved before and after Mermaid rendering.
pub const RESERVED_MERMAID_ROWS: u16 = 8;

/// Per-contribution Mermaid presentation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownMermaidPresentationState {
    /// Rendering has not started.
    Idle,
    /// A bounded worker is active.
    Rendering,
    /// Successful SVG output is available for image adaptation.
    Ready(Vec<u8>),
    /// Rendering failed with a visible typed diagnostic.
    Failed(String),
    /// BMUX cannot present images on this terminal.
    TerminalUnsupported,
}

impl MarkdownMermaidPresentationState {
    /// Return stable row reservation for every state.
    #[must_use]
    pub const fn reserved_rows(&self) -> u16 {
        RESERVED_MERMAID_ROWS
    }

    /// Return source-preserving terminal fallback text.
    #[must_use]
    pub fn fallback(&self, source: &str) -> String {
        match self {
            Self::Idle => format!("[Mermaid diagram idle]\n{source}"),
            Self::Rendering => format!("[Rendering Mermaid diagram…]\n{source}"),
            Self::Ready(_) => source.to_owned(),
            Self::Failed(diagnostic) => {
                format!("[Mermaid rendering failed: {diagnostic}]\n{source}")
            }
            Self::TerminalUnsupported => {
                format!("[Mermaid image unsupported; source follows]\n{source}")
            }
        }
    }
}

/// One resident Mermaid contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownMermaidInput {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Versioned renderer request cache key.
    pub cache_key: String,
    /// Source retained for fallback/source-view activation.
    pub source: String,
    /// Whether the contribution is visible or in bounded prefetch.
    pub may_render: bool,
}

#[derive(Debug)]
struct MermaidEntry {
    cache_key: String,
    source: String,
    state: MarkdownMermaidPresentationState,
}

/// Resident Mermaid state and cancellation ownership.
#[derive(Debug, Default)]
pub struct MarkdownMermaidPresentationStore {
    entries: BTreeMap<String, MermaidEntry>,
    workers: BTreeMap<String, MermaidCancellationToken>,
}

impl MarkdownMermaidPresentationStore {
    /// Reconcile the bounded resident projection and cancel stale workers.
    pub fn reconcile(&mut self, inputs: &[MarkdownMermaidInput], terminal_supported: bool) {
        let active_ids = inputs
            .iter()
            .map(|input| input.contribution_id.as_str())
            .collect::<BTreeSet<_>>();
        let active_keys = inputs
            .iter()
            .map(|input| input.cache_key.as_str())
            .collect::<BTreeSet<_>>();
        self.entries
            .retain(|id, _| active_ids.contains(id.as_str()));
        self.workers.retain(|key, cancellation| {
            if active_keys.contains(key.as_str()) {
                true
            } else {
                cancellation.cancel();
                false
            }
        });
        for input in inputs {
            let reset = self
                .entries
                .get(&input.contribution_id)
                .is_none_or(|entry| entry.cache_key != input.cache_key);
            if reset {
                self.entries.insert(
                    input.contribution_id.clone(),
                    MermaidEntry {
                        cache_key: input.cache_key.clone(),
                        source: input.source.clone(),
                        state: if terminal_supported {
                            MarkdownMermaidPresentationState::Idle
                        } else {
                            MarkdownMermaidPresentationState::TerminalUnsupported
                        },
                    },
                );
            }
        }
    }

    /// Start workers only for visible/bounded-prefetch idle contributions.
    #[must_use]
    pub fn schedule(&mut self, inputs: &[MarkdownMermaidInput]) -> Vec<MarkdownMermaidWork> {
        let mut work = Vec::new();
        for input in inputs.iter().filter(|input| input.may_render) {
            let Some(entry) = self.entries.get_mut(&input.contribution_id) else {
                continue;
            };
            if !matches!(entry.state, MarkdownMermaidPresentationState::Idle) {
                continue;
            }
            if self.workers.contains_key(&entry.cache_key) {
                continue;
            }
            let cancellation = MermaidCancellationToken::default();
            self.workers
                .insert(entry.cache_key.clone(), cancellation.clone());
            entry.state = MarkdownMermaidPresentationState::Rendering;
            work.push(MarkdownMermaidWork {
                contribution_id: input.contribution_id.clone(),
                cache_key: entry.cache_key.clone(),
                source: entry.source.clone(),
                cancellation,
            });
        }
        work
    }

    /// Hydrate unchanged requests from the bounded Mermaid cache.
    pub fn hydrate_from_cache(&mut self, cache: &mut MermaidRenderCache) {
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| matches!(entry.state, MarkdownMermaidPresentationState::Idle))
        {
            if let Some(rendered) = cache.get(&entry.cache_key) {
                let MermaidRenderedOutput::Svg(svg) = rendered.output;
                entry.state = MarkdownMermaidPresentationState::Ready(svg);
            }
        }
    }

    /// Complete one worker and update every owner sharing its cache key.
    pub fn complete(
        &mut self,
        key: &str,
        result: Result<MermaidRendered, MermaidRenderError>,
        cache: &mut MermaidRenderCache,
    ) {
        self.workers.remove(key);
        match result {
            Ok(rendered) => {
                cache.insert(rendered.clone());
                let MermaidRenderedOutput::Svg(svg) = rendered.output;
                for entry in self
                    .entries
                    .values_mut()
                    .filter(|entry| entry.cache_key == key)
                {
                    entry.state = MarkdownMermaidPresentationState::Ready(svg.clone());
                }
            }
            Err(error) => {
                for entry in self
                    .entries
                    .values_mut()
                    .filter(|entry| entry.cache_key == key)
                {
                    entry.state = MarkdownMermaidPresentationState::Failed(error.to_string());
                }
            }
        }
    }

    /// Return state for one resident contribution.
    #[must_use]
    pub fn state(&self, contribution_id: &str) -> Option<&MarkdownMermaidPresentationState> {
        self.entries.get(contribution_id).map(|entry| &entry.state)
    }

    /// Return source for source-view activation.
    #[must_use]
    pub fn source(&self, contribution_id: &str) -> Option<&str> {
        self.entries
            .get(contribution_id)
            .map(|entry| entry.source.as_str())
    }

    /// Return clipped BMUX destination geometry for a ready diagram.
    #[must_use]
    pub fn ready_placement(
        &self,
        contribution_id: &str,
        destination: Rect,
        clip: Rect,
    ) -> Option<(String, &[u8], Rect, Rect)> {
        let MarkdownMermaidPresentationState::Ready(svg) = self.state(contribution_id)? else {
            return None;
        };
        (!destination.intersection(clip).is_empty()).then(|| {
            (
                format!("markdown-mermaid:{contribution_id}"),
                svg.as_slice(),
                destination,
                clip,
            )
        })
    }
}

/// One newly scheduled Mermaid worker request.
#[derive(Debug, Clone)]
pub struct MarkdownMermaidWork {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Versioned request cache key.
    pub cache_key: String,
    /// Diagram source.
    pub source: String,
    /// Cancellation tied to resident ownership.
    pub cancellation: MermaidCancellationToken,
}

#[cfg(test)]
mod tests {
    use bcode_mermaid_render::{
        MermaidRenderCache, MermaidRenderError, MermaidRenderRequest, MermaidRendered,
        MermaidRenderedOutput,
    };
    use bmux_tui::geometry::Rect;

    use super::{
        MarkdownMermaidInput, MarkdownMermaidPresentationState, MarkdownMermaidPresentationStore,
        RESERVED_MERMAID_ROWS,
    };

    fn input(id: &str, key: &str, may_render: bool) -> MarkdownMermaidInput {
        MarkdownMermaidInput {
            contribution_id: id.to_owned(),
            cache_key: key.to_owned(),
            source: "flowchart LR\nA --> B".to_owned(),
            may_render,
        }
    }

    fn rendered(key: &str) -> MermaidRendered {
        MermaidRendered {
            output: MermaidRenderedOutput::Svg(b"<svg/>".to_vec()),
            cache_key: key.to_owned(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn lifecycle_is_stable_bounded_and_source_preserving() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let inputs = [input("hidden", "k1", false), input("visible", "k2", true)];
        store.reconcile(&inputs, true);
        assert_eq!(store.schedule(&inputs).len(), 1);
        assert_eq!(
            store.state("hidden"),
            Some(&MarkdownMermaidPresentationState::Idle)
        );
        assert_eq!(
            store.state("visible"),
            Some(&MarkdownMermaidPresentationState::Rendering)
        );
        let mut cache = MermaidRenderCache::default();
        store.complete("k2", Ok(rendered("k2")), &mut cache);
        assert!(matches!(
            store.state("visible"),
            Some(MarkdownMermaidPresentationState::Ready(_))
        ));
        assert_eq!(
            store.state("visible").expect("state").reserved_rows(),
            RESERVED_MERMAID_ROWS
        );
        assert_eq!(store.source("visible"), Some("flowchart LR\nA --> B"));
        assert!(
            store
                .ready_placement("visible", Rect::new(2, 3, 20, 8), Rect::new(4, 4, 10, 4),)
                .is_some()
        );
    }

    #[test]
    fn stale_workers_cancel_cache_reuses_and_failures_stay_visible() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let active = [input("diagram", "key", true)];
        store.reconcile(&active, true);
        let work = store.schedule(&active).pop().expect("scheduled work");
        store.reconcile(&[], true);
        assert!(work.cancellation.is_cancelled());

        let mut cache = MermaidRenderCache::default();
        cache.insert(rendered("cached"));
        let cached = [input("reconstructed", "cached", true)];
        store.reconcile(&cached, true);
        store.hydrate_from_cache(&mut cache);
        assert!(matches!(
            store.state("reconstructed"),
            Some(MarkdownMermaidPresentationState::Ready(_))
        ));
        assert!(store.schedule(&cached).is_empty());

        let failed = [input("failed", "failed", true)];
        store.reconcile(&failed, true);
        assert_eq!(store.schedule(&failed).len(), 1);
        store.complete(
            "failed",
            Err(MermaidRenderError::InvalidDiagram {
                message: "bad diagram".to_owned(),
            }),
            &mut cache,
        );
        let state = store.state("failed").expect("failure state");
        assert!(matches!(state, MarkdownMermaidPresentationState::Failed(_)));
        assert!(state.fallback("flowchart").contains("bad diagram"));
        assert!(state.fallback("flowchart").contains("flowchart"));
    }

    #[test]
    fn renderer_key_changes_reset_state_and_unsupported_terminals_keep_source() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let first = [input("diagram", &request.cache_key(), true)];
        let mut store = MarkdownMermaidPresentationStore::default();
        store.reconcile(&first, true);
        assert_eq!(store.schedule(&first).len(), 1);

        let changed = [input("diagram", "mermaid-v-next", true)];
        store.reconcile(&changed, false);
        let state = store.state("diagram").expect("unsupported state");
        assert_eq!(
            state,
            &MarkdownMermaidPresentationState::TerminalUnsupported
        );
        assert!(state.fallback("flowchart LR").contains("flowchart LR"));
        assert!(store.schedule(&changed).is_empty());
    }
}
