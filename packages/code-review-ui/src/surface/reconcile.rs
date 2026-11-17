//! Keeping a stack review in step with its stack.
//!
//! A stack review follows the provider-confirmed stack, not a fixed list of pull requests. It
//! holds the stack two ways: one source per layer (each pull request against its parent) and one
//! whole-stack source (trunk to the top layer, one diff), so the view can switch between them
//! without creating another review. A review that names only one of the two gets the other when
//! the stack is confirmed. New layers join in stack order, while closed or merged layers leave
//! without losing their comments or viewed marks, and the whole-stack source follows the stack's
//! top. Ordinary pull-request sources remain explicit, fixed selections.

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

/// A whole-stack source's provider, repository and top pull request number.
fn whole_of(source: &ReviewSource) -> Option<(&str, &str, u64)> {
    match &source.kind {
        ReviewSourceKind::Stack {
            provider,
            repository,
            top,
        } => Some((provider, repository, *top)),
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

/// The label of a whole-stack source whose top layer is `top`.
fn whole_label(stack: &bcode_code_review_models::ReviewStack) -> String {
    stack.layers.last().map_or_else(String::new, |top| {
        format!(
            "Stack #{} · {} ({} layers)",
            top.pull.number,
            top.pull.title,
            stack.layers.len()
        )
    })
}

/// What reconciling a review's sources with its stack changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StackChange {
    /// The review's sources after.
    pub sources: Vec<ReviewSource>,
    /// Layers that left: their pull requests, `#N`.
    pub removed: Vec<String>,
    /// Layers that joined: their pull requests, `#N`.
    pub added: Vec<String>,
    /// The review had no layer sources before: they were added so the stack can be shown one
    /// pull request at a time, not because the stack changed.
    pub layers_filled: bool,
}

/// The review's `sources` matched to `stack` (of `provider`'s `repository`): the layer sources of
/// that stack in its order, existing ones kept (their ids and labels), new ones added; layers in
/// `gone` that the stack no longer has removed; then its whole-stack source (kept, with its top
/// moved to the stack's top, or added). Other sources keep their place: the stack's sources take
/// the place of its first one. `None` when nothing changes.
#[must_use]
pub fn reconcile(
    sources: &[ReviewSource],
    gone: &BTreeSet<String>,
    provider: &str,
    repository: &str,
    stack: &bcode_code_review_models::ReviewStack,
) -> Option<StackChange> {
    let numbers: Vec<u64> = stack.layers.iter().map(|layer| layer.pull.number).collect();
    let top = *numbers.last()?;
    let in_stack =
        |p: &str, r: &str, n: u64| p == provider && r == repository && numbers.contains(&n);
    let layer_ours = |source: &ReviewSource| {
        layer_of(source).is_some_and(|(p, r, n)| {
            p == provider && r == repository && (numbers.contains(&n) || gone.contains(&source.id))
        })
    };
    // The whole-stack source this stack keeps: the first one whose top is in the stack.
    let whole = sources
        .iter()
        .find(|source| whole_of(source).is_some_and(|(p, r, n)| in_stack(p, r, n)));
    let ours = |source: &ReviewSource| {
        layer_ours(source) || whole.is_some_and(|whole| whole.id == source.id)
    };
    // An unrelated stack in the same repository must not replace this review's sources.
    if whole.is_none()
        && !sources
            .iter()
            .any(|source| layer_of(source).is_some_and(|(p, r, n)| in_stack(p, r, n)))
    {
        return None;
    }
    let had_layers = sources.iter().any(layer_ours);
    let included = sources
        .iter()
        .filter(|source| ours(source))
        .any(|source| source.included);
    let removed: Vec<&ReviewSource> = sources
        .iter()
        .filter(|source| layer_ours(source) && gone.contains(&source.id))
        .filter(|source| layer_of(source).is_some_and(|(_, _, n)| !numbers.contains(&n)))
        .collect();
    let mut used: BTreeSet<String> = sources.iter().map(|source| source.id.clone()).collect();
    let mut stack_sources = Vec::new();
    let mut added = Vec::new();
    for layer in &stack.layers {
        let kept = sources.iter().find(|source| {
            layer_ours(source) && layer_of(source).is_some_and(|(_, _, n)| n == layer.pull.number)
        });
        if let Some(source) = kept {
            stack_sources.push(source.clone());
        } else {
            let id = free_id(&used);
            used.insert(id.clone());
            added.push(format!("#{}", layer.pull.number));
            stack_sources.push(ReviewSource {
                id,
                kind: ReviewSourceKind::StackLayer {
                    provider: provider.to_owned(),
                    repository: repository.to_owned(),
                    number: layer.pull.number,
                },
                label: format!("#{} {}", layer.pull.number, layer.pull.title),
                included,
            });
        }
    }
    let whole_kind = ReviewSourceKind::Stack {
        provider: provider.to_owned(),
        repository: repository.to_owned(),
        top,
    };
    stack_sources.push(match whole {
        Some(source) if source.kind == whole_kind => source.clone(),
        Some(source) => ReviewSource {
            kind: whole_kind,
            label: whole_label(stack),
            ..source.clone()
        },
        None => ReviewSource {
            id: free_id(&used),
            kind: whole_kind,
            label: whole_label(stack),
            included,
        },
    });
    let mut result = Vec::new();
    let mut placed = false;
    for source in sources {
        if ours(source) {
            if !placed {
                result.append(&mut stack_sources);
                placed = true;
            }
            continue;
        }
        result.push(source.clone());
    }
    if result == sources {
        return None;
    }
    Some(StackChange {
        sources: result,
        removed: removed
            .iter()
            .filter_map(|source| layer_of(source).map(|(_, _, number)| format!("#{number}")))
            .collect(),
        added: if had_layers { added } else { Vec::new() },
        layers_filled: !had_layers,
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

    /// Match the review's layers to the stack it follows. The review is saved and opened again
    /// when layers join, leave, or change order.
    pub(super) fn reconcile_stack(&mut self) {
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
        if change.layers_filled {
            // A review made of the whole-stack diff keeps showing it, now with layers to switch
            // to.
            self.stack_view = super::stack::StackView::Whole;
            self.workspace_mut()
                .presentation
                .get_or_insert_with(Default::default)
                .set_stack_view(super::stack::StackView::Whole);
        }
        self.request_save();
        // Opening again with the new sources is not a refresh to announce.
        self.switching = true;
        self.reopen_pending = true;
        if change.removed.is_empty() && change.added.is_empty() {
            // Only the other way of showing the stack joined, or the stack's top moved along.
            return;
        }
        let mut body = Vec::new();
        if !change.removed.is_empty() {
            body.push(format!(
                "{} closed or merged (removed)",
                change.removed.join(", ")
            ));
        }
        if !change.added.is_empty() {
            body.push(format!("{} added", change.added.join(", ")));
        }
        // Stays until read: it says why layers the reviewer knew are gone.
        self.toasts.push_sticky(
            ToastSeverity::Info,
            "The stack changed",
            Some(format!("{}.", body.join("; "))),
        );
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

    fn whole_source(id: &str, top: u64) -> ReviewSource {
        ReviewSource {
            id: id.to_owned(),
            kind: ReviewSourceKind::Stack {
                provider: "fake".to_owned(),
                repository: "o/r".to_owned(),
                top,
            },
            label: format!("Stack #{top}"),
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
        // The whole stack joins after the layers, so the view can switch to it.
        assert_eq!(
            ids,
            ["source-9", "source-1", "source-10", "source-3", "source-11"]
        );
        assert_eq!(whole_of(&change.sources[4]), Some(("fake", "o/r", 12)));
        assert_eq!(change.removed, ["#11"]);
        assert_eq!(change.added, ["#13"]);
        assert!(!change.layers_filled);
        // Kept layers keep their labels.
        assert_eq!(change.sources[1].label, "#10 old title");
    }

    #[test]
    fn explicit_pull_requests_and_unrelated_stacks_stay_fixed() {
        let mut pull = layer_source("source-1", 10);
        pull.kind = ReviewSourceKind::PullRequest {
            provider: "fake".to_owned(),
            repository: "o/r".to_owned(),
            number: 10,
        };
        assert_eq!(
            reconcile(&[pull], &BTreeSet::new(), "fake", "o/r", &stack(&[10, 11])),
            None
        );
        let sources = vec![layer_source("source-1", 10), layer_source("source-2", 90)];
        let change =
            reconcile(&sources, &BTreeSet::new(), "fake", "o/r", &stack(&[10, 11])).unwrap();
        assert_eq!(change.sources.last(), Some(&sources[1]));
        assert_eq!(
            reconcile(&sources, &BTreeSet::new(), "fake", "o/r", &stack(&[80, 81])),
            None
        );
    }

    #[test]
    fn a_growing_stack_adds_layers_without_waiting_for_a_layer_to_leave() {
        let sources = vec![layer_source("source-1", 10)];
        let current = stack(&[10, 11]);
        let change = reconcile(&sources, &BTreeSet::new(), "fake", "o/r", &current).unwrap();
        assert_eq!(change.sources[0], sources[0]);
        assert_eq!(change.added, ["#11"]);
        assert_eq!(change.removed, Vec::<String>::new());
        assert_eq!(layer_of(&change.sources[1]), Some(("fake", "o/r", 11)));
        assert_eq!(
            reconcile(&change.sources, &BTreeSet::new(), "fake", "o/r", &current),
            None
        );
    }

    #[test]
    fn a_layer_that_failed_but_is_still_in_the_stack_is_not_removed() {
        let sources = vec![layer_source("source-1", 10), whole_source("source-2", 10)];
        let gone = BTreeSet::from(["source-1".to_owned()]);
        assert_eq!(
            reconcile(&sources, &gone, "fake", "o/r", &stack(&[10])),
            None
        );
    }

    #[test]
    fn a_whole_stack_gains_its_layers_quietly_and_keeps_its_id() {
        let sources = vec![whole_source("source-1", 12)];
        let change = reconcile(
            &sources,
            &BTreeSet::new(),
            "fake",
            "o/r",
            &stack(&[10, 11, 12]),
        )
        .unwrap();
        let layers: Vec<Option<u64>> = change
            .sources
            .iter()
            .map(|source| layer_of(source).map(|(_, _, number)| number))
            .collect();
        assert_eq!(layers, [Some(10), Some(11), Some(12), None]);
        assert_eq!(change.sources[3], sources[0]);
        // Not a change of the stack: nothing to announce.
        assert!(change.layers_filled);
        assert!(change.added.is_empty() && change.removed.is_empty());
        assert_eq!(
            reconcile(
                &change.sources,
                &BTreeSet::new(),
                "fake",
                "o/r",
                &stack(&[10, 11, 12])
            ),
            None
        );
    }

    #[test]
    fn the_whole_stack_follows_the_stacks_top() {
        let sources = vec![
            layer_source("source-1", 10),
            layer_source("source-2", 11),
            whole_source("source-3", 11),
        ];
        let change = reconcile(
            &sources,
            &BTreeSet::new(),
            "fake",
            "o/r",
            &stack(&[10, 11, 12]),
        )
        .unwrap();
        let whole = change.sources.last().expect("the whole stack");
        assert_eq!(whole.id, "source-3");
        assert_eq!(whole_of(whole), Some(("fake", "o/r", 12)));
        assert_eq!(change.added, ["#12"]);
        // A whole stack of another stack is not this one's.
        let unrelated = vec![whole_source("source-1", 90)];
        assert_eq!(
            reconcile(
                &unrelated,
                &BTreeSet::new(),
                "fake",
                "o/r",
                &stack(&[10, 11])
            ),
            None
        );
    }
}
