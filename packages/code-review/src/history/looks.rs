//! The "since you last looked" baseline: which version of each layer a comparison starts from,
//! chosen from the looks recorded as the reviewer works (see `docs/review-stack-history.md`).

use bcode_code_review_models::{
    LayerLook, LayerPair, LayerTimeline, LookKind, ReviewedRevision, VersionPoint,
};

/// Which looks count for the baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BaselineRule {
    /// Engaged looks (viewed, commented, asked, pass, reviewed); opened looks only when there
    /// are none.
    #[default]
    Engaged,
    /// Every look, opened ones included.
    Opened,
}

/// A layer's baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastLooked {
    /// The reviewer never looked at the layer: its whole change is new to them.
    NewToYou,
    /// The reviewer's newest look is at the current head: nothing is new.
    Unchanged(LayerLook),
    /// The reviewer last looked at this other version.
    Since(LayerLook),
}

impl LastLooked {
    /// The look the baseline comes from, when there is one.
    #[must_use]
    pub const fn look(&self) -> Option<&LayerLook> {
        match self {
            Self::NewToYou => None,
            Self::Unchanged(look) | Self::Since(look) => Some(look),
        }
    }
}

/// Layer `number`'s baseline from `looks` (any order, any layers) when its head is now
/// `current_head`: the newest look `rule` counts.
#[must_use]
pub fn last_looked(
    looks: &[LayerLook],
    number: u64,
    current_head: &str,
    rule: BaselineRule,
) -> LastLooked {
    let of_layer = || looks.iter().filter(move |look| look.number == number);
    let engaged = of_layer().any(|look| look.kind.engaged());
    let newest = of_layer()
        .filter(|look| rule == BaselineRule::Opened || !engaged || look.kind.engaged())
        .max_by(|a, b| (a.looked_at_ms, a.kind).cmp(&(b.looked_at_ms, b.kind)));
    match newest {
        None => LastLooked::NewToYou,
        Some(look) if look.head_oid == current_head => LastLooked::Unchanged(look.clone()),
        Some(look) => LastLooked::Since(look.clone()),
    }
}

/// Where `head`'s own change starts, as `layer`'s timeline knows it.
#[must_use]
pub fn fork_of(layer: &LayerTimeline, head: &str) -> Option<String> {
    layer
        .versions
        .iter()
        .rev()
        .find(|version| version.version.head_oid == head)
        .and_then(|version| version.fork_oid.clone())
}

/// The pair comparing `layer` from `baseline` to `current`: the baseline's fork point from its
/// look, else from the timeline.
#[must_use]
pub fn baseline_pair(
    layer: Option<&LayerTimeline>,
    number: u64,
    baseline: &LastLooked,
    current: VersionPoint,
) -> LayerPair {
    let from = baseline.look().map(|look| VersionPoint {
        head_oid: look.head_oid.clone(),
        fork_oid: look
            .fork_oid
            .clone()
            .or_else(|| layer.and_then(|layer| fork_of(layer, &look.head_oid))),
    });
    let current = VersionPoint {
        fork_oid: current
            .fork_oid
            .clone()
            .or_else(|| layer.and_then(|layer| fork_of(layer, &current.head_oid))),
        ..current
    };
    LayerPair {
        number,
        from,
        to: current,
    }
}

/// Looks seeded from what earlier reviews recorded: each layer's last completed pass
/// (`reviewed_revisions`) becomes a [`LookKind::Pass`] look at the time it was recorded.
/// Recording them again changes nothing.
#[must_use]
pub fn pass_looks(
    provider: &str,
    repository: &str,
    reviewed: &[(u64, ReviewedRevision)],
    review_id: Option<&str>,
) -> Vec<LayerLook> {
    reviewed
        .iter()
        .filter_map(|(number, revision)| {
            Some(LayerLook {
                provider: provider.to_owned(),
                repository: repository.to_owned(),
                number: *number,
                head_oid: revision.reviewed_head_oid.clone()?,
                fork_oid: revision.reviewed_base_oid.clone(),
                kind: LookKind::Pass,
                looked_at_ms: revision.recorded_at_ms.unwrap_or(0),
                review_id: review_id.map(str::to_owned),
            })
        })
        .collect()
}

/// Looks seeded from the reviewer's own reviews on the forge: `(number, commit, when)` becomes a
/// [`LookKind::Reviewed`] look.
#[must_use]
pub fn forge_review_looks(
    provider: &str,
    repository: &str,
    reviews: &[(u64, String, u64)],
) -> Vec<LayerLook> {
    reviews
        .iter()
        .filter(|(_, commit, _)| !commit.is_empty())
        .map(|(number, commit, at_ms)| LayerLook {
            provider: provider.to_owned(),
            repository: repository.to_owned(),
            number: *number,
            head_oid: commit.clone(),
            fork_oid: None,
            kind: LookKind::Reviewed,
            looked_at_ms: *at_ms,
            review_id: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(number: u64, head: &str, kind: LookKind, at: u64) -> LayerLook {
        LayerLook {
            provider: "github".into(),
            repository: "o/r".into(),
            number,
            head_oid: head.into(),
            fork_oid: None,
            kind,
            looked_at_ms: at,
            review_id: None,
        }
    }

    #[test]
    fn engaged_looks_beat_newer_opened_looks() {
        let looks = [
            look(1, "v1", LookKind::Viewed, 100),
            look(1, "v2", LookKind::Opened, 200),
            look(2, "w1", LookKind::Opened, 100),
        ];
        assert_eq!(
            last_looked(&looks, 1, "v3", BaselineRule::Engaged),
            LastLooked::Since(looks[0].clone())
        );
        // With opened looks counting, the glance at v2 is the baseline.
        assert_eq!(
            last_looked(&looks, 1, "v3", BaselineRule::Opened),
            LastLooked::Since(looks[1].clone())
        );
        // Only opened looks: they count.
        assert_eq!(
            last_looked(&looks, 2, "w2", BaselineRule::Engaged),
            LastLooked::Since(looks[2].clone())
        );
    }

    #[test]
    fn a_look_at_the_current_head_means_unchanged_and_none_means_new() {
        let looks = [
            look(1, "v1", LookKind::Pass, 100),
            look(1, "v2", LookKind::Commented, 300),
        ];
        assert_eq!(
            last_looked(&looks, 1, "v2", BaselineRule::Engaged),
            LastLooked::Unchanged(looks[1].clone())
        );
        assert_eq!(
            last_looked(&looks, 3, "x", BaselineRule::Engaged),
            LastLooked::NewToYou
        );
    }

    #[test]
    fn seeded_looks_come_from_passes_and_forge_reviews() {
        let revision = ReviewedRevision {
            reviewed_base_oid: Some("base".into()),
            reviewed_head_oid: Some("head".into()),
            recorded_at_ms: Some(42),
        };
        let looks = pass_looks(
            "github",
            "o/r",
            &[
                (7, revision),
                (
                    8,
                    ReviewedRevision {
                        reviewed_base_oid: None,
                        reviewed_head_oid: None,
                        recorded_at_ms: None,
                    },
                ),
            ],
            Some("w"),
        );
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].kind, LookKind::Pass);
        assert_eq!(looks[0].fork_oid.as_deref(), Some("base"));
        assert_eq!(looks[0].looked_at_ms, 42);
        let reviewed = forge_review_looks(
            "github",
            "o/r",
            &[(7, "c".into(), 5), (8, String::new(), 6)],
        );
        assert_eq!(reviewed.len(), 1);
        assert_eq!(reviewed[0].kind, LookKind::Reviewed);
    }
}
