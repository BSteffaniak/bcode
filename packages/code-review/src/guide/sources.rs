//! Guide-local projections of review groups. The workspace still owns a single commits source;
//! only the guide's revision pairs, labels and citations are keyed by selector.

use bcode_code_review_models::{
    Citation, GuideHead, ResolvedSource, ResolvedSourceMode, ReviewGroupKey, ReviewLineSide,
    ReviewSource, ReviewSourceKind,
};
use std::collections::{BTreeMap, BTreeSet};

/// Inputs to citation checking, freshness and `lifecycle::heads_of`, in review order.
#[derive(Debug, Default)]
pub struct GuideSources {
    /// Resolutions whose local `source_id` is the guide selector, not a new workspace source.
    pub sources: Vec<ResolvedSource>,
    /// Group display labels by selector.
    pub labels: BTreeMap<String, String>,
    /// Real PR numbers only.
    pub numbers: BTreeMap<String, u64>,
    /// Canonical workspace id by selector, for accepting unambiguous source aliases.
    pub owners: BTreeMap<String, String>,
}

impl GuideSources {
    /// Project resolved groups when starting a guide, or restore the exact revision pairs of
    /// a saved guide. `resolved` supplies the resolution(s) for each workspace source and
    /// `series` the commits sources that resolved as multi-member series (group identity is
    /// [`ReviewGroupKey::for_resolved`]); a commits source's other pinned representations need
    /// no replay. A legacy plain id addresses its saved base/head, not whichever commit
    /// happens to be visible now.
    #[must_use]
    pub fn project(
        workspace: &[ReviewSource],
        resolved: &[ResolvedSource],
        series: &BTreeSet<String>,
        pins: Option<&BTreeMap<String, GuideHead>>,
    ) -> Self {
        let mut out = Self::default();
        let mut seen = BTreeSet::new();
        for template in resolved {
            let Some(source) = workspace.iter().find(|s| s.id == template.source_id) else {
                continue;
            };
            let key =
                ReviewGroupKey::for_resolved(template, series.contains(&source.id)).selector();
            let pin = match pins {
                Some(heads) => match heads.get(&key) {
                    Some(head) => Some(head),
                    None => continue,
                },
                None => None,
            };
            if seen.insert(key.clone()) {
                out.add(source, template, key, pin);
            }
        }
        if let Some(pins) = pins {
            for source in workspace {
                let template = resolved.iter().find(|r| r.source_id == source.id);
                for (key, pin) in pins {
                    if seen.contains(key) {
                        continue;
                    }
                    let commits = matches!(source.kind, ReviewSourceKind::Commits { .. });
                    if key == &source.id
                        || (commits
                            && (key == &combined(&source.id)
                                || pin
                                    .head
                                    .commit_oid()
                                    .is_some_and(|oid| key == &member(&source.id, oid))))
                    {
                        // Hidden PR layers and commit groups are unrestricted revision pairs.
                        // Never invent browse selectors or constraints for a missing source.
                        let hidden;
                        let template = if let Some(template) = template {
                            template
                        } else if matches!(
                            source.kind,
                            ReviewSourceKind::Commits { .. }
                                | ReviewSourceKind::PullRequest { .. }
                                | ReviewSourceKind::StackLayer { .. }
                                | ReviewSourceKind::Stack { .. }
                        ) {
                            hidden = ResolvedSource {
                                source_id: source.id.clone(),
                                base: pin.base.clone(),
                                head: pin.head.clone(),
                                mode: ResolvedSourceMode::Diff,
                                paths: Vec::new(),
                                line_range: None,
                                part: None,
                                selection: None,
                            };
                            &hidden
                        } else {
                            continue;
                        };
                        out.add(source, template, key.clone(), Some(pin));
                        seen.insert(key.clone());
                    }
                }
            }
        }
        out
    }

    fn add(
        &mut self,
        source: &ReviewSource,
        template: &ResolvedSource,
        key: String,
        pin: Option<&GuideHead>,
    ) {
        let mut resolved = template.clone();
        let label = if let Some(pin) = pin {
            resolved.base.clone_from(&pin.base);
            resolved.head.clone_from(&pin.head);
            // Position is a presentation hint, never identity. A hidden group's template
            // may have been borrowed from another member.
            resolved.part = None;
            pin.label.clone()
        } else {
            template
                .part
                .as_ref()
                .map_or_else(|| source.label.clone(), |p| p.label.clone())
        };
        resolved.source_id.clone_from(&key);
        self.sources.push(resolved);
        self.labels.insert(key.clone(), label);
        if let ReviewSourceKind::PullRequest { number, .. }
        | ReviewSourceKind::StackLayer { number, .. } = &source.kind
        {
            self.numbers.insert(key.clone(), *number);
        }
        self.owners.insert(key, source.id.clone());
    }
}

/// Add plain-id aliases for legacy commit pins only when an active pair has the same
/// base and head. Never map a legacy citation to the first visible member by position.
#[must_use]
pub fn with_legacy_pairs(
    mut active: Vec<ResolvedSource>,
    workspace: &[ReviewSource],
    pins: &BTreeMap<String, GuideHead>,
) -> Vec<ResolvedSource> {
    for source in workspace {
        if !matches!(source.kind, ReviewSourceKind::Commits { .. }) {
            continue;
        }
        let Some(pin) = pins.get(&source.id) else {
            continue;
        };
        if active.iter().any(|r| r.source_id == source.id) {
            continue;
        }
        let member = pin.head.commit_oid().map(|oid| member(&source.id, oid));
        let combined = combined(&source.id);
        if let Some(group) = active.iter().find(|r| {
            r.head == pin.head
                && r.base == pin.base
                && (r.source_id == combined || member.as_ref() == Some(&r.source_id))
        }) {
            let mut legacy = group.clone();
            legacy.source_id.clone_from(&source.id);
            active.push(legacy);
        }
    }
    active
}

/// Regenerate a collected synthetic commit before reading a saved guide. Never substitute
/// the newly resolved selection if it does not reproduce the recorded revision pair.
///
/// # Errors
/// * The repository cannot be read, or replay cannot reproduce the exact saved pair.
pub fn restore_objects(
    data: &crate::ReviewData,
    root: &std::path::Path,
    sources: &[ReviewSource],
    heads: &BTreeMap<String, GuideHead>,
) -> Result<(), crate::data::DataError> {
    for source in sources {
        if !crate::session::may_be_commit_series(source) {
            continue;
        }
        let Some(pin) = heads.get(&combined(&source.id)) else {
            continue;
        };
        let Some(oid) = pin.head.commit_oid() else {
            continue;
        };
        if crate::history::present_commits(root, &BTreeSet::from([oid]))?.contains(oid) {
            continue;
        }
        let plan = data.commit_series(root, source, true)?;
        if !plan
            .combined
            .is_some_and(|r| r.base == pin.base && r.head == pin.head)
        {
            return Err(crate::data::DataError::NotFound(format!(
                "cannot reproduce pinned combined revision {oid}: {}",
                plan.unavailable.as_deref().unwrap_or("selection changed")
            )));
        }
    }
    Ok(())
}

/// Selector a citation reads: its own source id, else a legacy fallback.
///
/// For a legacy citation without a source id, the `primary` group when that is an ordinary source (as such citations were always read), else
/// the one saved pin whose cited side has the citation's revision. A legacy citation is never
/// mapped to a series group by position.
#[must_use]
pub fn citation_selector_or_primary<'a>(
    citation: &'a Citation,
    heads: &'a BTreeMap<String, GuideHead>,
    primary: Option<&'a str>,
) -> Option<&'a str> {
    if citation.source_id.is_none()
        && let Some(primary) = primary.filter(|primary| !is_series_selector(primary))
    {
        return Some(primary);
    }
    citation_selector(citation, heads)
}

/// Whether `selector` names a member or the combined group of a series rather than an
/// ordinary source.
#[must_use]
pub fn is_series_selector(selector: &str) -> bool {
    ReviewGroupKey::source_id_of_selector(selector) != selector
}

/// Exact selector of a citation. Legacy repository-context citations lacked a source id;
/// infer one only when the cited side's revision identifies exactly one saved pin.
#[expect(
    clippy::must_use_candidate,
    reason = "Option results do not need an additional must_use attribute"
)]
pub fn citation_selector<'a>(
    citation: &'a Citation,
    heads: &'a BTreeMap<String, GuideHead>,
) -> Option<&'a str> {
    if let Some(id) = citation.source_id.as_deref() {
        return Some(id);
    }
    let mut matches = heads.iter().filter(|(_, head)| {
        let revision = match citation.side {
            ReviewLineSide::New => &head.head,
            ReviewLineSide::Old => &head.base,
        };
        revision == &citation.revision
    });
    let (id, _) = matches.next()?;
    matches.next().is_none().then_some(id.as_str())
}

fn member(source_id: &str, oid: &str) -> String {
    ReviewGroupKey::Member {
        source_id: source_id.to_owned(),
        member: oid.to_owned(),
    }
    .selector()
}

fn combined(source_id: &str) -> String {
    ReviewGroupKey::Combined {
        source_id: source_id.to_owned(),
    }
    .selector()
}
