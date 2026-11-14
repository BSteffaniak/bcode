//! Keeping a step-through stack review in step with its stack.
//!
//! A step-through review saves one source per layer, pinned to its pull request. After a
//! restack, some of those pull requests are closed or merged (their layers fail with
//! [`REVIEW_DIAGNOSTIC_PULL_GONE`]) and new ones may have joined. Those failures are not errors to
//! show: they are held here until the stack is known again (stack news from a layer still open),
//! then the review's layers are made to match it. Layers that left are removed, new ones are
//! added in stack order, the review is saved and opened again, and one notice says what changed.
//! Layers that left keep their comments and viewed marks in the store; only the review stops
//! showing them.

use super::ReviewSurface;
use bcode_code_review_models::{
    REVIEW_DIAGNOSTIC_PULL_GONE, ReviewSource, ReviewSourceDiagnostic, ReviewSourceKind,
};
use bmux_tui_components::toast_stack::ToastSeverity;
use std::collections::BTreeSet;

/// A layer source's provider, repository and pull request number.
fn layer_of(source: &ReviewSource) -> Option<(&str, &str, u64)> {
    match &source.kind {
        ReviewSourceKind::StackLayer {
            provider,
            repository,
            number,
        } => Some((provider, repository, *number)),
        _ => None,
    }
}

/// A source id not in `used`: `source-N` with the lowest free `N` past the ones in use.
fn free_id(used: &BTreeSet<String>) -> String {
    let mut next = used
        .iter()
        .filter_map(|id| id.strip_prefix("source-")?.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    while used.contains(&format!("source-{next}")) {
        next += 1;
    }
    format!("source-{next}")
}

/// What reconciling a review's layers with its stack changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StackChange {
    /// The review's sources after.
    pub sources: Vec<ReviewSource>,
    /// Layers that left: their pull requests, `#N`.
    pub removed: Vec<String>,
    /// Layers that joined: their pull requests, `#N`.
    pub added: Vec<String>,
}

/// The review's `sources` matched to `stack` (of `provider`'s `repository`): the layer sources of
/// that stack in its order, existing ones kept (their ids and labels), new ones added; layers in
/// `gone` that the stack no longer has removed. Other sources keep their place: the stack's
/// layers take the place of its first layer source. `None` when nothing changes.
#[must_use]
pub fn reconcile(
    sources: &[ReviewSource],
    gone: &BTreeSet<String>,
    provider: &str,
    repository: &str,
    stack: &bcode_code_review_models::ReviewStack,
) -> Option<StackChange> {
    let ours = |source: &ReviewSource| {
        layer_of(source).is_some_and(|(p, r, _)| p == provider && r == repository)
    };
    let numbers: Vec<u64> = stack.layers.iter().map(|layer| layer.pull.number).collect();
    let removed: Vec<&ReviewSource> = sources
        .iter()
        .filter(|source| ours(source) && gone.contains(&source.id))
        .filter(|source| layer_of(source).is_some_and(|(_, _, n)| !numbers.contains(&n)))
        .collect();
    // Only a review some of whose layers left is changed: a stack that grew while every layer
    // still opens is the user's to add (the review may follow only part of it on purpose).
    if removed.is_empty() {
        return None;
    }
    let mut used: BTreeSet<String> = sources.iter().map(|source| source.id.clone()).collect();
    let mut layers = Vec::new();
    let mut added = Vec::new();
    for layer in &stack.layers {
        let kept = sources.iter().find(|source| {
            ours(source) && layer_of(source).is_some_and(|(_, _, n)| n == layer.pull.number)
        });
        if let Some(source) = kept {
            layers.push(source.clone());
        } else {
            let id = free_id(&used);
            used.insert(id.clone());
            let label = format!("#{} {}", layer.pull.number, layer.pull.title);
            added.push(format!("#{}", layer.pull.number));
            layers.push(ReviewSource {
                id,
                kind: ReviewSourceKind::StackLayer {
                    provider: provider.to_owned(),
                    repository: repository.to_owned(),
                    number: layer.pull.number,
                },
                label,
                included: true,
            });
        }
    }
    let mut result = Vec::new();
    let mut placed = false;
    for source in sources {
        if ours(source) {
            if !placed {
                result.append(&mut layers);
                placed = true;
            }
            continue;
        }
        result.push(source.clone());
    }
    Some(StackChange {
        sources: result,
        removed: removed
            .iter()
            .filter_map(|source| layer_of(source).map(|(_, _, number)| format!("#{number}")))
            .collect(),
        added,
    })
}

impl ReviewSurface {
    /// Whether `diagnostic` is a stack layer whose pull request left the stack: held for
    /// reconciling instead of shown.
    pub(super) fn hold_gone_layer(&mut self, diagnostic: &ReviewSourceDiagnostic) -> bool {
        let layer = self
            .sources
            .iter()
            .any(|source| source.id == diagnostic.source_id && layer_of(source).is_some());
        if diagnostic.code != REVIEW_DIAGNOSTIC_PULL_GONE || !layer {
            return false;
        }
        self.gone_layers.insert(diagnostic.source_id.clone());
        self.reconcile_stack();
        true
    }

    /// Match the review's layers to the stack it follows, once both a layer that left and the
    /// stack are known. The review is saved and opened again with the new layers.
    pub(super) fn reconcile_stack(&mut self) {
        if self.gone_layers.is_empty() {
            return;
        }
        let Some(news) = self.stack_news.as_ref() else {
            return;
        };
        let Some(change) = reconcile(
            &self.sources,
            &self.gone_layers,
            &news.provider,
            &news.repository,
            &news.stack,
        ) else {
            return;
        };
        self.gone_layers.clear();
        self.sources.clone_from(&change.sources);
        self.workspace_mut().sources = change.sources;
        self.request_save();
        let mut body = vec![format!(
            "Since it was saved: {} closed or merged (removed)",
            change.removed.join(", ")
        )];
        if !change.added.is_empty() {
            body.push(format!("{} added", change.added.join(", ")));
        }
        // Stays until read: it says why layers the reviewer knew are gone.
        self.toasts.push_sticky(
            ToastSeverity::Info,
            "The stack changed",
            Some(format!("{}.", body.join("; "))),
        );
        // Opening again with the new layers is not a refresh to announce.
        self.switching = true;
        self.reopen_pending = true;
    }

    /// Layers that left with no stack to match them to (every layer it knew of left, say): one
    /// notice naming them.
    pub(super) fn report_gone_layers(&mut self) {
        if self.gone_layers.is_empty() || self.stack_news.is_some() {
            return;
        }
        let labels: Vec<String> = self
            .sources
            .iter()
            .filter(|source| self.gone_layers.contains(&source.id))
            .map(|source| source.label.clone())
            .collect();
        self.gone_layers.clear();
        self.toasts.push_sticky(
            ToastSeverity::Warning,
            "Some layers are closed or merged",
            Some(format!(
                "{}. Edit the review's sources to remove them.",
                labels.join(", ")
            )),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::{PullRequestSummary, ReviewStack, ReviewStackLayer};

    fn layer_source(id: &str, number: u64) -> ReviewSource {
        ReviewSource {
            id: id.to_owned(),
            kind: ReviewSourceKind::StackLayer {
                provider: "fake".to_owned(),
                repository: "o/r".to_owned(),
                number,
            },
            label: format!("#{number} old title"),
            included: true,
        }
    }

    fn stack(numbers: &[u64]) -> ReviewStack {
        ReviewStack {
            trunk: "main".to_owned(),
            rooted_at_default_branch: true,
            layers: numbers
                .iter()
                .map(|number| ReviewStackLayer {
                    pull: PullRequestSummary {
                        number: *number,
                        title: format!("Layer {number}"),
                        author: None,
                        base_branch: String::new(),
                        head_branch: String::new(),
                        head_oid: String::new(),
                        base_oid: None,
                        review_requested_from_me: false,
                        draft: false,
                        updated: None,
                        url: None,
                    },
                    parent: None,
                })
                .collect(),
            requested: numbers[0],
            siblings: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn layers_that_left_go_new_ones_come_in_stack_order_and_others_stay() {
        let other = ReviewSource {
            id: "source-9".to_owned(),
            kind: ReviewSourceKind::WorkingTreeUnstaged,
            label: "Unstaged".to_owned(),
            included: true,
        };
        let sources = vec![
            other,
            layer_source("source-1", 10),
            layer_source("source-2", 11),
            layer_source("source-3", 12),
        ];
        // 11 was closed; 13 joined between 10 and 12.
        let gone = BTreeSet::from(["source-2".to_owned()]);
        let change =
            reconcile(&sources, &gone, "fake", "o/r", &stack(&[10, 13, 12])).expect("a change");
        let ids: Vec<&str> = change.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["source-9", "source-1", "source-10", "source-3"]);
        assert_eq!(change.removed, ["#11"]);
        assert_eq!(change.added, ["#13"]);
        // Kept layers keep their labels.
        assert_eq!(change.sources[1].label, "#10 old title");
    }

    #[test]
    fn nothing_changes_without_a_layer_that_left() {
        let sources = vec![layer_source("source-1", 10)];
        assert_eq!(
            reconcile(&sources, &BTreeSet::new(), "fake", "o/r", &stack(&[10, 11])),
            None
        );
        // A layer that failed but is still in the stack is not removed.
        let gone = BTreeSet::from(["source-1".to_owned()]);
        assert_eq!(
            reconcile(&sources, &gone, "fake", "o/r", &stack(&[10])),
            None
        );
    }
}
