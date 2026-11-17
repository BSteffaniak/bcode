//! Guided-review operations behind the `code_review.guide.*` tools (see
//! `docs/code-review-guide.md`).
//!
//! They start a guide or version, write it in batches under the session's grant, read it (with
//! freshness), check citations without writing, plan it, and export it as markdown.
//!
//! A session may write a guide version as its grant allows: the grant its review binding
//! carries for that guide, or without one, everything on the versions it started itself.

use super::code::{label_of, source_number};
use super::{AssistantError, OpenReview, ReviewTools};
use crate::data::grep;
use crate::guide::cite::{ReviewDataReader, check_citations};
use crate::guide::config::{GuideConfig, StrategyChoice};
use crate::guide::context::{GuideContext, read_context};
use crate::guide::export::{ExportOptions, LinkStyle, render_markdown};
use crate::guide::freshness::assess_version;
use crate::guide::lint::{LintItem, lint, lint_section};
use crate::guide::plan::{GuidePlan, PlanFile, PlanInput, PlanLayer, plan};
use crate::guide::template::{ResolvedTemplate, resolve_template};
use crate::guide::write::{
    ApplyOutcome, AreaStatusInput, Doc, GuideOp, MAX_OPS, OUTSIDE_GRANT, OpError, OutlineInput,
    SectionWrite, Writer, apply, apply_area_sections, batch_citations,
};
use crate::store::StoreError;
use bcode_code_review_models::{
    CitationCheck, CitationCheckStatus, CitationRequest, CreateReviewGuideRequest,
    ForkGuideVersionRequest, GetGuideVersionRequest, GrantMode, GuideArea, GuideAreaStatus,
    GuideDepth, GuideGrant, GuideHead, GuideOrigin, GuideRecord, GuideScope, GuideStatus,
    GuideStrategy, GuideVersion, GuideVersionRecord, ListGuideAreasRequest,
    ListGuideSectionMarksRequest, ListGuideThreadsRequest, ListGuideVersionsRequest,
    ListReviewGuidesRequest, REVIEW_GUIDE_SCHEMA_VERSION, ResolvedSource, ResolvedSourceMode,
    ReviewCompareBaseline, ReviewGuide, ReviewSourceKind, SaveGuideAreaRequest,
    UpdateReviewGuideRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Bytes of a `guide.get` answer aimed at, below the tools' output bound.
pub const GUIDE_GET_BUDGET: usize = 120 * 1024;
/// Citations one check takes at most.
pub const MAX_CHECK_CITATIONS: usize = 100;
/// Package manifests whose directories are package roots.
const MANIFESTS: [&str; 3] = ["Cargo.toml", "package.json", "pyproject.toml"];
/// Bytes of a guide's recorded directions kept.
const MAX_DIRECTIONS_BYTES: usize = 16 * 1024;
/// Package manifests a plan reads at most.
const MAX_MANIFESTS: usize = 5_000;

/// Refusal code: the version changed since the revision the writer named.
pub const GUIDE_CONFLICT: &str = "conflict";
/// Refusal code: a strict write had citation problems.
pub const GUIDE_CITATION_PROBLEMS: &str = "citation_problems";
/// Refusal code: the document the batch would write is over a bound or inconsistent.
pub const GUIDE_INVALID_DOCUMENT: &str = "invalid_document";

/// What a guide covers, as a writer names it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScopeInput {
    /// `review` (default), `stack`, `layer`, or `since`.
    pub kind: Option<String>,
    /// The layer, for `layer` (`#102` or a source id).
    pub layer: Option<String>,
    /// The comparison baseline, for `since`.
    pub baseline: Option<ReviewCompareBaseline>,
}

/// How `guide.start` starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartMode {
    /// A new guide (or, when one exists for the scope, a new version of it).
    #[default]
    New,
    /// A new version of an existing guide.
    NewVersion,
    /// Continue writing an existing version.
    Resume,
}

/// A `guide.start` request.
#[derive(Debug, Clone, Default)]
pub struct GuideStart {
    /// The guide, for a new version or resume.
    pub guide_id: Option<String>,
    /// What it covers.
    pub scope: Option<ScopeInput>,
    /// How to start.
    pub mode: StartMode,
    /// The version to resume, or a new version is based on.
    pub version: Option<u32>,
    /// Whether a new version starts with a copy of the version it is based on.
    pub copy: bool,
    /// Title.
    pub title: Option<String>,
    /// Depth.
    pub depth: Option<GuideDepth>,
    /// Strategy the version is written with.
    pub strategy: Option<GuideStrategy>,
    /// Template.
    pub template: Option<String>,
    /// The user's directions.
    pub directions: Option<String>,
    /// A planned outline.
    pub outline: Vec<OutlineInput>,
}

/// What `guide.write` did: written, or refused with nothing written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideWriteOutcome {
    /// Written.
    Written(Value),
    /// Refused; nothing was written.
    Refused(Value),
}

/// What `guide.get` includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideInclude {
    /// The section tree.
    Outline,
    /// Section bodies, findings and checks.
    Bodies,
    /// Citations used by the included sections.
    Citations,
    /// Each citation's freshness against the review now, and the stale sections.
    Freshness,
    /// Fan-out areas.
    Areas,
    /// Ask threads.
    Threads,
    /// Every version of the guide.
    Versions,
    /// Sections the reviewer marked reviewed.
    Marks,
}

/// A `guide.get` request.
#[derive(Debug, Clone, Default)]
pub struct GuideGet {
    /// The guide; the review's only guide when absent.
    pub guide_id: Option<String>,
    /// The version; the current one when absent.
    pub version: Option<u32>,
    /// Sections whose bodies to include; every section when empty.
    pub sections: Vec<String>,
    /// What to include; outline, bodies and citations when empty.
    pub include: Vec<GuideInclude>,
}

/// A `guide.export` request.
#[derive(Debug, Clone, Default)]
pub struct GuideExport {
    /// The guide; the review's only guide when absent.
    pub guide_id: Option<String>,
    /// The version; the current one when absent.
    pub version: Option<u32>,
    /// `markdown`.
    pub format: Option<String>,
    /// Sections to export; every section when empty.
    pub sections: Vec<String>,
    /// Whether unverified citations are included (default true).
    pub include_unverified: Option<bool>,
    /// `permalink` (default) or `relative`.
    pub links: Option<String>,
}

/// The context a call declared and may read.
#[derive(Debug, Clone, Default)]
pub struct DeclaredContext {
    /// Repository-relative context files the call declared as reads.
    pub paths: Vec<String>,
    /// What in the configuration could not be used.
    pub problems: Vec<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn encode<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Sources pinned to a version's heads, in review order; labels and pull request numbers by
/// source id.
struct Pinned {
    sources: Vec<ResolvedSource>,
    labels: BTreeMap<String, String>,
    numbers: BTreeMap<String, u64>,
}

impl ReviewTools<'_> {
    /// The review's guides, most recently changed first (archived ones too).
    fn guides(&self, review: &OpenReview) -> Result<Vec<GuideRecord>, AssistantError> {
        let (target, scope) = review.scope();
        Ok(self
            .store
            .list_guides(&ListReviewGuidesRequest {
                repo_path: review.repo_root.clone(),
                target,
                scope: Some(scope),
                include_archived: true,
            })?
            .guides)
    }

    /// The guide `guide_id` names among the review's, or its only unarchived guide.
    fn guide(
        &self,
        review: &OpenReview,
        guide_id: Option<&str>,
    ) -> Result<GuideRecord, AssistantError> {
        let guides = self.guides(review)?;
        if let Some(id) = guide_id.map(str::trim).filter(|id| !id.is_empty()) {
            return guides
                .into_iter()
                .find(|record| record.guide.guide_id == id)
                .ok_or_else(|| {
                    AssistantError::NotFound(format!(
                        "this review has no guide {id} (code_review.guide.get without guide_id lists its guides)"
                    ))
                });
        }
        let live: Vec<GuideRecord> = guides
            .into_iter()
            .filter(|record| record.guide.archived_at_ms.is_none())
            .collect();
        match live.len() {
            0 => Err(AssistantError::NotFound(
                "this review has no guide yet; start one with code_review.guide.start".to_owned(),
            )),
            1 => Ok(live.into_iter().next().ok_or_else(|| {
                AssistantError::NotFound("this review has no guide yet".to_owned())
            })?),
            _ => Err(AssistantError::Invalid(format!(
                "this review has several guides; pass guide_id: {}",
                live.iter()
                    .map(|record| format!("{} ({})", record.guide.guide_id, record.guide.title))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    fn version_record(
        &self,
        review: &OpenReview,
        guide_id: &str,
        version: Option<u32>,
    ) -> Result<GuideVersionRecord, AssistantError> {
        self.store
            .guide_version(&GetGuideVersionRequest {
                repo_path: review.repo_root.clone(),
                guide_id: guide_id.to_owned(),
                version,
            })?
            .version
            .ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "guide {guide_id} has no version {}",
                    version.map_or_else(|| "current".to_owned(), |version| version.to_string())
                ))
            })
    }

    /// The review's sources pinned to `heads` (sources the review no longer has are left out).
    fn pinned(review: &OpenReview, heads: Option<&BTreeMap<String, GuideHead>>) -> Pinned {
        let mut seen = BTreeSet::new();
        let sources = review
            .resolved
            .iter()
            .filter(|source| seen.insert(source.source_id.clone()))
            .filter_map(|source| {
                heads.map_or_else(
                    || Some(source.clone()),
                    |heads| {
                        heads.get(&source.source_id).map(|head| ResolvedSource {
                            base: head.base.clone(),
                            head: head.head.clone(),
                            ..source.clone()
                        })
                    },
                )
            })
            .collect();
        let mut labels = BTreeMap::new();
        let mut numbers = BTreeMap::new();
        for (index, source) in review.resolved.iter().enumerate() {
            labels.insert(source.source_id.clone(), label_of(review, index));
            if let Some(number) = review
                .workspace
                .sources
                .iter()
                .find(|candidate| candidate.id == source.source_id)
                .and_then(|candidate| source_number(&candidate.kind))
            {
                numbers.insert(source.source_id.clone(), number);
            }
        }
        Pinned {
            sources,
            labels,
            numbers,
        }
    }

    fn check(
        &self,
        review: &OpenReview,
        heads: Option<&BTreeMap<String, GuideHead>>,
        requests: &[CitationRequest],
    ) -> Vec<CitationCheck> {
        if requests.is_empty() {
            return Vec::new();
        }
        let pinned = Self::pinned(review, heads);
        let reader = ReviewDataReader::new(
            self.data,
            review.repo_root.clone(),
            &pinned.sources,
            &pinned.labels,
            &pinned.numbers,
        );
        check_citations(&reader, requests, now_ms())
    }

    /// The heads the review's sources pin now, by source id (the first part of a source that
    /// resolves to several).
    fn heads(review: &OpenReview) -> BTreeMap<String, GuideHead> {
        let mut heads = BTreeMap::new();
        for (index, source) in review.resolved.iter().enumerate() {
            heads
                .entry(source.source_id.clone())
                .or_insert_with(|| GuideHead {
                    base: source.base.clone(),
                    head: source.head.clone(),
                    label: label_of(review, index),
                });
        }
        heads
    }

    fn without_diff(review: &OpenReview) -> bool {
        !review.resolved.is_empty()
            && review
                .resolved
                .iter()
                .all(|source| source.mode == ResolvedSourceMode::Browse)
    }

    fn scope_of(
        review: &OpenReview,
        input: Option<&ScopeInput>,
    ) -> Result<GuideScope, AssistantError> {
        let kind = input
            .and_then(|input| input.kind.as_deref())
            .map_or("review", str::trim);
        Ok(match kind {
            "" | "review" => GuideScope::Review,
            "stack" => GuideScope::Stack {
                layers: Self::stepped_layers(review)
                    .into_iter()
                    .map(|layer| layer.source_id)
                    .collect(),
            },
            "layer" => {
                let name = input
                    .and_then(|input| input.layer.as_deref())
                    .ok_or_else(|| {
                        AssistantError::Invalid(
                            "a layer guide needs `scope.layer` (#102 or a source id)".to_owned(),
                        )
                    })?;
                let index = Self::pick_layer(review, name)?;
                GuideScope::Layer {
                    source_id: review.resolved[index].source_id.clone(),
                }
            }
            "since" => GuideScope::Since {
                baseline: input.and_then(|input| input.baseline).unwrap_or_default(),
                pairs: Vec::new(),
            },
            other => {
                return Err(AssistantError::Invalid(format!(
                    "unknown scope kind {other:?}; use review, stack, layer, or since"
                )));
            }
        })
    }

    fn pick_layer(review: &OpenReview, name: &str) -> Result<usize, AssistantError> {
        let number = name.trim().trim_start_matches('#').parse::<u64>().ok();
        review
            .resolved
            .iter()
            .enumerate()
            .position(|(index, source)| {
                source.source_id == name.trim()
                    || label_of(review, index) == name.trim()
                    || review
                        .workspace
                        .sources
                        .iter()
                        .find(|candidate| candidate.id == source.source_id)
                        .and_then(|candidate| source_number(&candidate.kind))
                        .is_some_and(|found| Some(found) == number)
            })
            .ok_or_else(|| AssistantError::NotFound(format!("this review has no layer {name}")))
    }

    /// The review's stack layers stepped one at a time, bottom first.
    fn stepped_layers(review: &OpenReview) -> Vec<PlanLayer> {
        let mut seen = BTreeSet::new();
        review
            .resolved
            .iter()
            .enumerate()
            .filter(|(_, source)| {
                review.workspace.sources.iter().any(|candidate| {
                    candidate.id == source.source_id
                        && matches!(candidate.kind, ReviewSourceKind::StackLayer { .. })
                })
            })
            .filter(|(_, source)| seen.insert(source.source_id.clone()))
            .map(|(index, source)| PlanLayer {
                source_id: source.source_id.clone(),
                label: label_of(review, index),
            })
            .collect()
    }

    /// The mode `session` may write `version` of `guide` with, or why it may not.
    fn writer_mode(
        review: &OpenReview,
        session: Option<&str>,
        guide: &ReviewGuide,
        version: &GuideVersion,
    ) -> Result<GrantMode, String> {
        grant_mode(review.guide_grant.as_ref(), session, guide, version)
    }

    fn template(
        review: &OpenReview,
        config: &GuideConfig,
        name: Option<&str>,
    ) -> Result<ResolvedTemplate, AssistantError> {
        resolve_template(config, name, Self::without_diff(review)).map_err(AssistantError::Invalid)
    }

    fn context(
        &self,
        review: &OpenReview,
        config: &GuideConfig,
        declared: &DeclaredContext,
        template: &str,
    ) -> GuideContext {
        let head = review.resolved.last().map(|source| &source.head);
        read_context(
            self.data,
            &review.repo_root,
            config,
            &declared.paths,
            head,
            template,
            declared.problems.clone(),
        )
    }

    /// Start a guide, a new version of one, or resume writing a version.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Refused`] with `outside_grant` when the session's grant does not let
    ///   it start or resume this guide's versions.
    /// * [`AssistantError::Invalid`] / [`AssistantError::NotFound`] for arguments that name
    ///   nothing the review has, or an outline that cannot be planned.
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read or written.
    pub fn guide_start(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        start: &GuideStart,
        config: &GuideConfig,
        declared: &DeclaredContext,
    ) -> Result<Value, AssistantError> {
        let scope = Self::scope_of(review, start.scope.as_ref())?;
        let existing = match start.guide_id.as_deref() {
            Some(id) => Some(self.guide(review, Some(id))?),
            None => self.guides(review)?.into_iter().find(|record| {
                record.guide.archived_at_ms.is_none() && record.guide.scope == scope
            }),
        };
        let mode = match (start.mode, &existing) {
            (StartMode::New, Some(_)) => StartMode::NewVersion,
            (StartMode::NewVersion | StartMode::Resume, None) => {
                return Err(AssistantError::Invalid(
                    "pass guide_id to continue a guide (code_review.guide.get lists the review's guides)".to_owned(),
                ));
            }
            (mode, _) => mode,
        };
        if let (Some(record), Some(grant)) = (&existing, &review.guide_grant)
            && grant.guide_id == record.guide.guide_id
            && !matches!(grant.mode, GrantMode::Generate | GrantMode::Restructure)
        {
            return Err(AssistantError::Refused {
                code: OUTSIDE_GRANT,
                message: format!(
                    "this conversation may not start or resume versions of guide {} ({}); ask the user",
                    record.guide.guide_id,
                    grant_summary(&grant.mode)
                ),
            });
        }
        let template = Self::template(review, config, start.template.as_deref())?;
        let context = self.context(review, config, declared, &template.name);
        let now = now_ms();
        let origin = GuideOrigin {
            session_id: session.map(str::to_owned),
            model: None,
            effort: None,
            strategy: start.strategy,
            depth: start.depth.unwrap_or(config.depth),
            template: Some(template.name.clone()),
            directions: start
                .directions
                .as_deref()
                .map(str::trim)
                .filter(|directions| !directions.is_empty())
                .map(|directions| crate::guide::config::cut(directions, MAX_DIRECTIONS_BYTES)),
            context_digest: Some(context.digest.clone()),
        };
        let (guide, record, note) = match (mode, existing) {
            (StartMode::Resume, Some(guide)) => {
                let number = start.version.unwrap_or(guide.guide.current_version);
                let record = self.resume(review, session, &guide, number, now)?;
                (
                    guide,
                    record,
                    format!("resumed version {number}; this conversation now writes it"),
                )
            }
            (StartMode::NewVersion, Some(guide)) => {
                self.fork(review, session, start, &guide, origin, now)?
            }
            (_, _) => self.create(review, session, start, scope, origin, now)?,
        };
        let version = &record.version;
        Ok(json!({
            "message": note,
            "guide_id": guide.guide.guide_id,
            "title": guide.guide.title,
            "version": version.version,
            "revision": record.revision,
            "status": version.status.as_str(),
            "based_on": version.based_on,
            "heads": version.heads,
            "grant": {"mode": "generate", "version": version.version},
            "outline": outline_view(&version.outline),
            "template": template,
            "context": context,
            "next": "write sections with code_review.guide.write (one batch per section or two, citing lines with quotes), then set_status complete",
        }))
    }

    /// Mark version `number` of `guide` generating again, written by `session` from now on.
    fn resume(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        guide: &GuideRecord,
        number: u32,
        now: u64,
    ) -> Result<GuideVersionRecord, AssistantError> {
        let session = session.map(str::to_owned);
        let resumed = self.store.update_guide_version(
            &review.repo_root,
            guide.guide.guide_id.clone(),
            Some(number),
            None,
            move |version: &mut GuideVersion| -> Result<(), String> {
                version.status = GuideStatus::Generating;
                version.origin.session_id.clone_from(&session);
                version.updated_at_ms = now;
                Ok(())
            },
        )?;
        resumed
            .map(|(record, ())| record)
            .map_err(AssistantError::Invalid)
    }

    /// Fork a new current version of `guide`, based on the version `start` names (its current
    /// one by default), pinned to the review's heads now.
    fn fork(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        start: &GuideStart,
        guide: &GuideRecord,
        origin: GuideOrigin,
        now: u64,
    ) -> Result<(GuideRecord, GuideVersionRecord, String), AssistantError> {
        let based_on = start.version.unwrap_or(guide.guide.current_version);
        let base = self.version_record(review, &guide.guide.guide_id, Some(based_on))?;
        let mut draft = new_version(&guide.guide.guide_id, Self::heads(review), origin, now);
        draft.based_on = Some(based_on);
        if start.copy {
            draft.outline = base.version.outline;
            draft.citations = base.version.citations;
            draft.retired_sections = base.version.retired_sections;
        }
        plan_outline(&mut draft, &start.outline, session)?;
        let forked = self.store.fork_guide_version(ForkGuideVersionRequest {
            repo_path: review.repo_root.clone(),
            version: draft,
            make_current: true,
        })?;
        let number = forked.version.version.version;
        let mut guide = forked.guide;
        if let Some(title) = start
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty() && *title != guide.guide.title)
        {
            let mut renamed = guide.guide.clone();
            title.clone_into(&mut renamed.title);
            renamed.updated_at_ms = now;
            if let Ok(response) = self.store.update_guide(UpdateReviewGuideRequest {
                repo_path: review.repo_root.clone(),
                guide: renamed.clone(),
                expected_revision: guide.revision,
            }) {
                guide = GuideRecord {
                    guide: renamed,
                    revision: response.revision,
                };
            }
        }
        let note = format!(
            "started version {number} based on version {based_on}{}",
            if start.copy {
                " (a copy of its sections)"
            } else {
                ""
            }
        );
        Ok((guide, forked.version, note))
    }

    /// Create a guide of `scope` with its first version.
    fn create(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        start: &GuideStart,
        scope: GuideScope,
        origin: GuideOrigin,
        now: u64,
    ) -> Result<(GuideRecord, GuideVersionRecord, String), AssistantError> {
        let guide_id = new_guide_id(review, session, now);
        let mut draft = new_version(&guide_id, Self::heads(review), origin, now);
        plan_outline(&mut draft, &start.outline, session)?;
        let title = start
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map_or_else(|| review.workspace.title.clone(), str::to_owned);
        let (target, review_scope) = review.scope();
        let created = self.store.create_guide(CreateReviewGuideRequest {
            repo_path: review.repo_root.clone(),
            target,
            scope: Some(review_scope),
            guide: ReviewGuide {
                guide_id,
                review_key: String::new(),
                scope,
                parent_guide_id: None,
                title: crate::guide::config::cut(
                    &title,
                    bcode_code_review_models::MAX_GUIDE_TITLE_BYTES,
                ),
                current_version: 1,
                created_at_ms: now,
                updated_at_ms: now,
                archived_at_ms: None,
            },
            version: draft,
        })?;
        let note = if review.is_transient() {
            "started a guide of an unsaved review; it is kept while the review is open and moves with it when the review is saved"
        } else {
            "started a guide"
        };
        Ok((created.guide, created.version, note.to_owned()))
    }

    /// Apply a batch of write operations to a guide version.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] / [`AssistantError::NotFound`] for a batch that is empty or
    ///   too long, or a guide or version the review does not have.
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read or written. Refusals (grant, conflicts, strict citation problems,
    ///   invalid documents) are returned as [`GuideWriteOutcome::Refused`].
    #[allow(clippy::too_many_arguments)]
    pub fn guide_write(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        guide_id: &str,
        version: Option<u32>,
        expected_revision: Option<u64>,
        strict: bool,
        ops: Vec<GuideOp>,
        config: &GuideConfig,
    ) -> Result<GuideWriteOutcome, AssistantError> {
        if ops.is_empty() || ops.len() > MAX_OPS {
            return Err(AssistantError::Invalid(format!(
                "pass 1 to {MAX_OPS} operations in `ops`"
            )));
        }
        let guide = self.guide(review, Some(guide_id))?;
        let number = version
            .or_else(|| {
                review
                    .guide_grant
                    .as_ref()
                    .filter(|grant| grant.guide_id == guide.guide.guide_id)
                    .and_then(|grant| grant.version)
            })
            .unwrap_or(guide.guide.current_version);
        let current = self.version_record(review, &guide.guide.guide_id, Some(number))?;
        if ops
            .iter()
            .any(|op| matches!(op, GuideOp::AreaResult { .. }))
        {
            return self.write_area(review, session, &guide, &current.version, &ops);
        }
        let requests = batch_citations(&ops);
        let checks = self.check(review, Some(&current.version.heads), &requests);
        if strict
            && checks
                .iter()
                .any(|check| check.status == CitationCheckStatus::Problem)
        {
            return Ok(GuideWriteOutcome::Refused(json!({
                "written": false,
                "code": GUIDE_CITATION_PROBLEMS,
                "message": "strict: citations have problems, so nothing was written; fix them (see each problem's message and suggestion) and write again",
                "citations": numbered_checks(&checks),
            })));
        }
        let grant = review.guide_grant.clone();
        let caller = session.map(str::to_owned);
        let guide_row = guide.guide;
        let now = now_ms();
        let written = self.store.update_guide_version(
            &review.repo_root,
            guide_row.guide_id.clone(),
            Some(number),
            expected_revision,
            move |version: &mut GuideVersion| -> Result<ApplyOutcome, Refusal> {
                let mode = grant_mode(grant.as_ref(), caller.as_deref(), &guide_row, version)
                    .map_err(Refusal::Grant)?;
                let writer = Writer {
                    mode: &mode,
                    session_id: caller.as_deref(),
                    now_ms: now,
                };
                apply(version, &ops, &checks, &writer).map_err(Refusal::Ops)
            },
        );
        let (record, outcome) = match written {
            Ok(Ok(written)) => written,
            Ok(Err(Refusal::Grant(message))) => {
                return Ok(refused_op(OUTSIDE_GRANT, message));
            }
            Ok(Err(Refusal::Ops(errors))) => {
                return Ok(GuideWriteOutcome::Refused(json!({
                    "written": false,
                    "message": "nothing was written; fix the operations named and send the batch again",
                    "errors": errors,
                })));
            }
            Err(error) => return store_refusal(error, "version"),
        };
        let template =
            Self::template(review, config, record.version.origin.template.as_deref()).ok();
        let lint = template
            .as_ref()
            .map(|template| lint(&record.version, template))
            .unwrap_or_default();
        Ok(GuideWriteOutcome::Written(json!({
            "written": true,
            "guide_id": record.version.guide_id,
            "version": record.version.version,
            "revision": record.revision,
            "status": record.version.status.as_str(),
            "sections_written": outcome.sections_written,
            "citations": outcome.citations.iter().map(citation_view).collect::<Vec<_>>(),
            "lint": lint,
        })))
    }

    fn write_area(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        guide: &GuideRecord,
        version: &GuideVersion,
        ops: &[GuideOp],
    ) -> Result<GuideWriteOutcome, AssistantError> {
        let [
            GuideOp::AreaResult {
                area_id,
                sections,
                status,
                message,
            },
        ] = ops
        else {
            return Ok(refused_op(
                crate::guide::write::INVALID_OP,
                "area_result is sent alone in its own write",
            ));
        };
        let mode = match Self::writer_mode(review, session, &guide.guide, version) {
            Ok(mode) => mode,
            Err(message) => return Ok(refused_op(OUTSIDE_GRANT, message)),
        };
        let fine = match &mode {
            GrantMode::Area { area_id: granted } => granted == area_id,
            GrantMode::Generate | GrantMode::Restructure => true,
            _ => false,
        };
        if !fine {
            return Ok(refused_op(
                OUTSIDE_GRANT,
                format!(
                    "this conversation may not write the area {area_id} ({})",
                    grant_summary(&mode)
                ),
            ));
        }
        let requests = batch_citations(ops);
        let checks = self.check(review, Some(&version.heads), &requests);
        let now = now_ms();
        let (mut area, expected) = self.area(review, session, version, area_id, now)?;
        let mut outcome = ApplyOutcome::default();
        let mut retired = Vec::new();
        let mut doc = Doc {
            outline: &mut area.outline,
            citations: &mut area.citations,
            retired: &mut retired,
        };
        if let Err((code, message)) = apply_area_sections(&mut doc, sections, &checks, &mut outcome)
        {
            return Ok(refused_op(code, message));
        }
        area.status = status.map_or(GuideAreaStatus::Complete, AreaStatusInput::status);
        area.message.clone_from(message);
        area.updated_at_ms = now;
        let saved = match self.store.save_guide_area(SaveGuideAreaRequest {
            repo_path: review.repo_root.clone(),
            area,
            expected_revision: expected,
        }) {
            Ok(saved) => saved,
            Err(error) => return store_refusal(error, "area"),
        };
        Ok(GuideWriteOutcome::Written(json!({
            "written": true,
            "guide_id": guide.guide.guide_id,
            "version": version.version,
            "area_id": area_id,
            "revision": saved.revision,
            "sections_written": outcome.sections_written,
            "citations": outcome.citations.iter().map(citation_view).collect::<Vec<_>>(),
        })))
    }

    /// Area `area_id` of `version` and its revision, or a new running area of `session`.
    fn area(
        &self,
        review: &OpenReview,
        session: Option<&str>,
        version: &GuideVersion,
        area_id: &str,
        now: u64,
    ) -> Result<(GuideArea, Option<u64>), AssistantError> {
        let existing = self
            .store
            .list_guide_areas(&ListGuideAreasRequest {
                repo_path: review.repo_root.clone(),
                guide_id: version.guide_id.clone(),
                version: version.version,
            })?
            .areas
            .into_iter()
            .find(|record| record.area.area_id == area_id);
        Ok(match existing {
            Some(record) => (record.area, Some(record.revision)),
            None => (
                GuideArea {
                    guide_id: version.guide_id.clone(),
                    version: version.version,
                    area_id: area_id.to_owned(),
                    session_id: session.map(str::to_owned),
                    status: GuideAreaStatus::Running,
                    message: None,
                    outline: Vec::new(),
                    citations: BTreeMap::new(),
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                None,
            ),
        })
    }

    /// Read a guide, bounded at [`GUIDE_GET_BUDGET`] with what was cut named; without a guide
    /// id, a review with several guides lists them.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when the review has no such guide or version.
    /// * [`AssistantError::Store`] when state cannot be read.
    pub fn guide_get(&self, review: &OpenReview, get: &GuideGet) -> Result<Value, AssistantError> {
        if get.guide_id.is_none()
            && let Some(listing) = self.guide_listing(review)?
        {
            return Ok(listing);
        }
        let guide = self.guide(review, get.guide_id.as_deref())?;
        let record = self.version_record(review, &guide.guide.guide_id, get.version)?;
        let version = &record.version;
        let include: BTreeSet<GuideInclude> = if get.include.is_empty() {
            [
                GuideInclude::Outline,
                GuideInclude::Bodies,
                GuideInclude::Citations,
            ]
            .into()
        } else {
            get.include.iter().copied().collect()
        };
        let mut answer = json!({
            "guide": {
                "guide_id": guide.guide.guide_id,
                "title": guide.guide.title,
                "scope": guide.guide.scope,
                "current_version": guide.guide.current_version,
                "archived": guide.guide.archived_at_ms.is_some(),
            },
            "version": {
                "version": version.version,
                "revision": record.revision,
                "status": version.status.as_str(),
                "message": version.status.message(),
                "based_on": version.based_on,
                "origin": version.origin,
                "heads": version.heads,
            },
        });
        let mut cut = self.add_extras(review, version, &include, &mut answer)?;
        fit_sections(version, &get.sections, &include, &mut answer, &mut cut);
        if !cut.is_empty() {
            answer["cut"] = json!(cut);
        }
        Ok(answer)
    }

    /// The review's guides, unless it has exactly one unarchived guide.
    fn guide_listing(&self, review: &OpenReview) -> Result<Option<Value>, AssistantError> {
        let guides = self.guides(review)?;
        let live = guides
            .iter()
            .filter(|record| record.guide.archived_at_ms.is_none())
            .count();
        if live == 1 {
            return Ok(None);
        }
        Ok(Some(json!({
            "guides": guides.iter().map(|record| json!({
                "guide_id": record.guide.guide_id,
                "title": record.guide.title,
                "scope": record.guide.scope,
                "current_version": record.guide.current_version,
                "archived": record.guide.archived_at_ms.is_some(),
            })).collect::<Vec<_>>(),
            "message": if guides.is_empty() {
                "this review has no guide yet; start one with code_review.guide.start"
            } else {
                "pass guide_id to read one"
            },
        })))
    }

    /// Add what `include` asks for besides sections and citations: freshness, versions, areas,
    /// threads, marks. Returns what was cut to stay within the budget.
    fn add_extras(
        &self,
        review: &OpenReview,
        version: &GuideVersion,
        include: &BTreeSet<GuideInclude>,
        answer: &mut Value,
    ) -> Result<Vec<String>, AssistantError> {
        let repo = &review.repo_root;
        let id = &version.guide_id;
        if include.contains(&GuideInclude::Freshness) {
            answer["freshness"] = match assess_version(self.data, repo, version, &review.resolved) {
                Ok(freshness) => encode(&freshness),
                Err(error) => json!({"error": error.to_string()}),
            };
        }
        if include.contains(&GuideInclude::Versions) {
            answer["versions"] = encode(
                &self
                    .store
                    .list_guide_versions(&ListGuideVersionsRequest {
                        repo_path: repo.clone(),
                        guide_id: id.clone(),
                    })?
                    .versions,
            );
        }
        if include.contains(&GuideInclude::Areas) {
            answer["areas"] = encode(
                &self
                    .store
                    .list_guide_areas(&ListGuideAreasRequest {
                        repo_path: repo.clone(),
                        guide_id: id.clone(),
                        version: version.version,
                    })?
                    .areas,
            );
        }
        if include.contains(&GuideInclude::Threads) {
            answer["threads"] = encode(
                &self
                    .store
                    .list_guide_threads(&ListGuideThreadsRequest {
                        repo_path: repo.clone(),
                        guide_id: id.clone(),
                        version: Some(version.version),
                    })?
                    .threads,
            );
        }
        if include.contains(&GuideInclude::Marks) {
            answer["marks"] = encode(
                &self
                    .store
                    .list_guide_section_marks(&ListGuideSectionMarksRequest {
                        repo_path: repo.clone(),
                        guide_id: id.clone(),
                    })?
                    .marks,
            );
        }
        let mut cut = Vec::new();
        if answer.to_string().len() > GUIDE_GET_BUDGET / 2 {
            for key in ["areas", "threads", "versions", "marks"] {
                if let Some(object) = answer.as_object_mut()
                    && object.remove(key).is_some()
                {
                    cut.push(format!("{key} (ask for them alone)"));
                }
                if answer.to_string().len() <= GUIDE_GET_BUDGET / 2 {
                    break;
                }
            }
        }
        Ok(cut)
    }

    /// Check citations, or a section, against a guide version's heads (the review's current
    /// heads without a guide) without writing anything.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for nothing to check, too many citations, or a section
    ///   that cannot be applied.
    /// * [`AssistantError::NotFound`] / [`AssistantError::Store`] for a guide or version the
    ///   review does not have, or state that cannot be read.
    pub fn guide_check(
        &self,
        review: &OpenReview,
        guide_id: Option<&str>,
        version: Option<u32>,
        citations: &[CitationRequest],
        section: Option<SectionWrite>,
        config: &GuideConfig,
    ) -> Result<Value, AssistantError> {
        if citations.is_empty() && section.is_none() {
            return Err(AssistantError::Invalid(
                "pass `citations` or `section` to check".to_owned(),
            ));
        }
        let section_citations = section.as_ref().map_or(0, |section| {
            batch_citations(&[GuideOp::PutSection(section.clone())]).len()
        });
        if citations.len() + section_citations > MAX_CHECK_CITATIONS {
            return Err(AssistantError::Invalid(format!(
                "at most {MAX_CHECK_CITATIONS} citations per check, counting the section's"
            )));
        }
        let pinned = if guide_id.is_some() || !self.guides(review)?.is_empty() {
            match self.guide(review, guide_id) {
                Ok(guide) => Some(self.version_record(review, &guide.guide.guide_id, version)?),
                Err(error) if guide_id.is_some() => return Err(error),
                Err(_) => None,
            }
        } else {
            None
        };
        let heads = pinned.as_ref().map(|record| &record.version.heads);
        let mut answer = json!({
            "pinned_to": heads.map_or_else(|| encode(&Self::heads(review)), encode),
            "citations": numbered_checks(&self.check(review, heads, citations)),
        });
        if let Some(section) = section {
            let op = GuideOp::PutSection(section);
            let requests = batch_citations(std::slice::from_ref(&op));
            let checks = self.check(review, heads, &requests);
            let mut scratch = pinned.map_or_else(
                || new_version("check", Self::heads(review), GuideOrigin::default(), 0),
                |record| record.version,
            );
            let id = match &op {
                GuideOp::PutSection(write) => write.id.clone(),
                _ => String::new(),
            };
            let applied = apply(
                &mut scratch,
                std::slice::from_ref(&op),
                &checks,
                &Writer {
                    mode: &GrantMode::Restructure,
                    session_id: None,
                    now_ms: 0,
                },
            );
            answer["section"] = match applied {
                Ok(_) => {
                    let lint: Vec<LintItem> = scratch
                        .section(&id)
                        .map(|section| lint_section(section, &scratch.citations))
                        .unwrap_or_default();
                    json!({
                        "id": id,
                        "citations": numbered_checks(&checks),
                        "lint": lint,
                        "template": Self::template(review, config, None).ok().map(|template| template.required_sections),
                    })
                }
                Err(errors) => {
                    json!({"id": id, "errors": errors, "citations": numbered_checks(&checks)})
                }
            };
        }
        Ok(answer)
    }

    /// Plan a guide of the review: size, strategy, partition, cost estimate inputs, template,
    /// and context.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for an unknown template.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn guide_plan(
        &self,
        review: &OpenReview,
        config: &GuideConfig,
        strategy: Option<StrategyChoice>,
        depth: Option<GuideDepth>,
        template: Option<&str>,
        declared: &DeclaredContext,
    ) -> Result<Value, AssistantError> {
        let template = Self::template(review, config, template)?;
        let context = self.context(review, config, declared, &template.name);
        let input = self.plan_input(review, context.included_bytes)?;
        let plan: GuidePlan = plan(&input, config, strategy, depth);
        Ok(json!({
            "plan": plan,
            "template": template,
            "context": context,
        }))
    }

    fn plan_input(
        &self,
        review: &OpenReview,
        context_bytes: usize,
    ) -> Result<PlanInput, AssistantError> {
        let entries = self.entries(review)?;
        let files = entries
            .iter()
            .map(|(index, entry)| PlanFile {
                path: entry.path.clone(),
                source_id: review.resolved[*index].source_id.clone(),
                changed_lines: u64::from(entry.additions) + u64::from(entry.deletions),
            })
            .collect();
        let package_roots = match review.resolved.last() {
            Some(source) => {
                grep::list_paths(&review.repo_root, &source.head, MAX_MANIFESTS, |path| {
                    let name = path.rsplit('/').next().unwrap_or(path);
                    MANIFESTS.contains(&name)
                })?
                .0
                .into_iter()
                .map(|path| {
                    path.rsplit_once('/')
                        .map_or_else(String::new, |(directory, _)| directory.to_owned())
                })
                .collect()
            }
            None => Vec::new(),
        };
        let layers = Self::stepped_layers(review);
        Ok(PlanInput {
            files,
            layers: if layers.len() >= 2 {
                layers
            } else {
                Vec::new()
            },
            package_roots,
            without_diff: Self::without_diff(review),
            context_bytes,
        })
    }

    /// A guide version as markdown.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for a format other than markdown or unknown link style.
    /// * [`AssistantError::NotFound`] / [`AssistantError::Store`] for a guide or version the
    ///   review does not have, or state that cannot be read.
    pub fn guide_export(
        &self,
        review: &OpenReview,
        export: &GuideExport,
    ) -> Result<String, AssistantError> {
        match export.format.as_deref().map(str::trim) {
            None | Some("" | "markdown") => {}
            Some(other) => {
                return Err(AssistantError::Invalid(format!(
                    "guides export as markdown only, not {other}"
                )));
            }
        }
        let links = match export.links.as_deref().map(str::trim) {
            None | Some("" | "permalink") => LinkStyle::Permalink,
            Some("relative") => LinkStyle::Relative,
            Some(other) => {
                return Err(AssistantError::Invalid(format!(
                    "`links` is permalink or relative, not {other}"
                )));
            }
        };
        let guide = self.guide(review, export.guide_id.as_deref())?;
        let record = self.version_record(review, &guide.guide.guide_id, export.version)?;
        if let Some(missing) = export
            .sections
            .iter()
            .find(|id| record.version.section(id).is_none())
        {
            return Err(AssistantError::NotFound(format!(
                "version {} has no section {missing}",
                record.version.version
            )));
        }
        let repositories = review
            .workspace
            .sources
            .iter()
            .filter_map(|source| match &source.kind {
                ReviewSourceKind::PullRequest {
                    provider,
                    repository,
                    ..
                }
                | ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    ..
                }
                | ReviewSourceKind::Stack {
                    provider,
                    repository,
                    ..
                } if provider == "github" => Some((
                    source.id.clone(),
                    format!("https://{}/{repository}", bcode_forge_host()),
                )),
                _ => None,
            })
            .collect();
        Ok(render_markdown(
            &record.version,
            &ExportOptions {
                title: &guide.guide.title,
                repositories,
                links,
                include_unverified: export.include_unverified.unwrap_or(true),
                sections: export.sections.clone(),
            },
        ))
    }
}

/// The web host of GitHub pull request sources.
const fn bcode_forge_host() -> &'static str {
    "github.com"
}

/// Add the outline, the bodies of the `wanted` sections (every section when empty) and the
/// citations they use, as far as they fit [`GUIDE_GET_BUDGET`]; what does not fit is named in
/// `cut`.
fn fit_sections(
    version: &GuideVersion,
    wanted: &[String],
    include: &BTreeSet<GuideInclude>,
    answer: &mut Value,
    cut: &mut Vec<String>,
) {
    let bodies = include.contains(&GuideInclude::Bodies);
    let mut cited: Vec<String> = Vec::new();
    let mut cut_bodies: Vec<String> = Vec::new();
    let mut used = answer.to_string().len();
    if include.contains(&GuideInclude::Outline) || bodies {
        let mut sections: Vec<Value> = Vec::new();
        for (depth, parent, section) in flatten(&version.outline) {
            let mut item = json!({
                "id": section.id,
                "depth": depth,
                "parent": parent,
                "kind": section.kind,
                "title": section.title,
                "state": section.state,
                "citations": section.citations,
            });
            if bodies && (wanted.is_empty() || wanted.contains(&section.id)) {
                let mut full = item.clone();
                full["body"] = json!(section.body);
                if !section.findings.is_empty() {
                    full["findings"] = encode(&section.findings);
                }
                if !section.checks.is_empty() {
                    full["checks"] = encode(&section.checks);
                }
                let full_bytes = full.to_string().len();
                if used + full_bytes <= GUIDE_GET_BUDGET {
                    item = full;
                    for cite in section.cited_ids() {
                        if !cited.iter().any(|known| known == cite) {
                            cited.push(cite.to_owned());
                        }
                    }
                } else {
                    cut_bodies.push(section.id.clone());
                }
            }
            used += item.to_string().len();
            sections.push(item);
        }
        answer["sections"] = Value::Array(sections);
    }
    if !cut_bodies.is_empty() {
        cut.push(format!(
            "bodies of {} (ask for them with `sections`)",
            cut_bodies.join(", ")
        ));
    }
    if include.contains(&GuideInclude::Citations) {
        if !bodies {
            cited = version.citations.keys().cloned().collect();
        }
        let mut citations = serde_json::Map::new();
        let mut cut_citations = Vec::new();
        for id in cited {
            if let Some(citation) = version.citations.get(&id) {
                let value = encode(citation);
                let value_bytes = value.to_string().len();
                if used + value_bytes <= GUIDE_GET_BUDGET {
                    used += value_bytes;
                    citations.insert(id, value);
                } else {
                    cut_citations.push(id);
                }
            }
        }
        answer["citations"] = Value::Object(citations);
        if !cut_citations.is_empty() {
            cut.push(format!(
                "citations {} (ask for fewer sections)",
                cut_citations.join(", ")
            ));
        }
    }
}

/// A write refused by one operation-level reason.
fn refused_op(code: &'static str, message: impl Into<String>) -> GuideWriteOutcome {
    GuideWriteOutcome::Refused(json!({
        "written": false,
        "errors": [{"op_index": 0, "code": code, "message": message.into()}],
    }))
}

/// A store failure of a guide write: conflicts and invalid documents are refusals the writer
/// can fix; anything else fails the call.
fn store_refusal(error: StoreError, record: &str) -> Result<GuideWriteOutcome, AssistantError> {
    match error {
        StoreError::RevisionConflict { current, .. } => Ok(GuideWriteOutcome::Refused(json!({
            "written": false,
            "code": GUIDE_CONFLICT,
            "current_revision": current,
            "message": format!("the {record} changed since that revision; read it again with code_review.guide.get and write again (or leave out expected_revision)"),
        }))),
        StoreError::Guide(error) => Ok(GuideWriteOutcome::Refused(json!({
            "written": false,
            "code": GUIDE_INVALID_DOCUMENT,
            "message": error.to_string(),
        }))),
        error => Err(error.into()),
    }
}

/// Why a guarded write did not apply.
enum Refusal {
    Grant(String),
    Ops(Vec<OpError>),
}

/// The mode `session` may write `version` of `guide` with: the binding's grant for the guide
/// (on its version, or the guide's current one), or without one, everything on versions the
/// session started.
fn grant_mode(
    grant: Option<&GuideGrant>,
    session: Option<&str>,
    guide: &ReviewGuide,
    version: &GuideVersion,
) -> Result<GrantMode, String> {
    if let Some(grant) = grant.filter(|grant| grant.guide_id == guide.guide_id) {
        let granted = grant.version.unwrap_or(guide.current_version);
        if granted != version.version {
            return Err(format!(
                "this conversation may only change version {granted} of guide {} ({})",
                guide.guide_id,
                grant_summary(&grant.mode)
            ));
        }
        return Ok(grant.mode.clone());
    }
    if session.is_some() && version.origin.session_id.as_deref() == session {
        return Ok(GrantMode::Generate);
    }
    Err(format!(
        "this conversation did not start version {} of guide {}; continue it with code_review.guide.start mode resume, or start a new version",
        version.version, guide.guide_id
    ))
}

fn grant_summary(mode: &GrantMode) -> String {
    match mode {
        GrantMode::Generate => "it writes this version".to_owned(),
        GrantMode::Area { area_id } => format!("it writes the area {area_id}"),
        GrantMode::Expand { section_id } => format!("it may only add under {section_id}"),
        GrantMode::Ask { section_id } => {
            format!("it may only add and promote answers under {section_id}")
        }
        GrantMode::Refresh { section_ids } => {
            format!("it may only rewrite {}", section_ids.join(", "))
        }
        GrantMode::Restructure => "it may restructure the guide".to_owned(),
        GrantMode::Verify => "it may only record claim checks".to_owned(),
    }
}

fn new_version(
    guide_id: &str,
    heads: BTreeMap<String, GuideHead>,
    origin: GuideOrigin,
    now: u64,
) -> GuideVersion {
    GuideVersion {
        guide_id: guide_id.to_owned(),
        version: 1,
        schema_version: REVIEW_GUIDE_SCHEMA_VERSION,
        heads,
        based_on: None,
        origin,
        status: GuideStatus::Generating,
        generation: None,
        outline: Vec::new(),
        citations: BTreeMap::new(),
        retired_sections: Vec::new(),
        created_at_ms: now,
        updated_at_ms: now,
    }
}

/// Plan `outline` into `draft` (a `set_outline` under the writer's own version).
fn plan_outline(
    draft: &mut GuideVersion,
    outline: &[OutlineInput],
    session: Option<&str>,
) -> Result<(), AssistantError> {
    if outline.is_empty() {
        return Ok(());
    }
    apply(
        draft,
        &[GuideOp::SetOutline {
            sections: outline.to_vec(),
        }],
        &[],
        &Writer {
            mode: &GrantMode::Generate,
            session_id: session,
            now_ms: draft.updated_at_ms,
        },
    )
    .map(|_| ())
    .map_err(|errors| {
        AssistantError::Invalid(format!(
            "the outline cannot be planned: {}",
            errors
                .iter()
                .map(|error| error.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })
}

fn new_guide_id(review: &OpenReview, session: Option<&str>, now: u64) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = Sha256::new();
    hasher.update(crate::scope::annotation_workspace_id(&review.workspace).as_bytes());
    hasher.update(session.unwrap_or_default().as_bytes());
    hasher.update(now.to_le_bytes());
    hasher.update(
        COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.update(std::process::id().to_le_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .take(8)
        .fold(String::from("g"), |mut id, byte| {
            use std::fmt::Write as _;
            let _ = write!(id, "{byte:02x}");
            id
        })
}

/// Every section with its depth and parent, depth first.
fn flatten(
    outline: &[bcode_code_review_models::Section],
) -> Vec<(usize, Option<&str>, &bcode_code_review_models::Section)> {
    fn walk<'a>(
        sections: &'a [bcode_code_review_models::Section],
        depth: usize,
        parent: Option<&'a str>,
        out: &mut Vec<(
            usize,
            Option<&'a str>,
            &'a bcode_code_review_models::Section,
        )>,
    ) {
        for section in sections {
            out.push((depth, parent, section));
            walk(&section.children, depth + 1, Some(&section.id), out);
        }
    }
    let mut out = Vec::new();
    walk(outline, 0, None, &mut out);
    out
}

/// The outline as ids, kinds, titles and states.
fn outline_view(outline: &[bcode_code_review_models::Section]) -> Vec<Value> {
    flatten(outline)
        .into_iter()
        .map(|(depth, parent, section)| {
            json!({
                "id": section.id,
                "depth": depth,
                "parent": parent,
                "kind": section.kind,
                "title": section.title,
                "state": section.state,
            })
        })
        .collect()
}

fn citation_view(outcome: &crate::guide::write::CitationOutcome) -> Value {
    let citation = &outcome.check.citation;
    json!({
        "id": outcome.id,
        "op_index": outcome.op_index,
        "status": outcome.check.status,
        "path": citation.path,
        "side": citation.side,
        "lines": citation.lines,
        "relation": citation.relation,
        "problems": outcome.check.problems,
        "warnings": outcome.check.warnings,
    })
}

fn numbered_checks(checks: &[CitationCheck]) -> Vec<Value> {
    checks
        .iter()
        .enumerate()
        .map(|(index, check)| {
            json!({
                "index": index,
                "status": check.status,
                "path": check.citation.path,
                "side": check.citation.side,
                "lines": check.citation.lines,
                "relation": check.citation.relation,
                "source": check.citation.source_id,
                "problems": check.problems,
                "warnings": check.warnings,
            })
        })
        .collect()
}
