//! A guide after new pushes and against comparisons, on the review screen (see
//! `docs/code-review-guide.md`): refreshing stale sections, reading older versions, and guides
//! of what changed since a comparison baseline.
//!
//! * **Refresh.** `r` refreshes the current section and `alt+r` every stale one. When the
//!   version on screen is pinned to older heads than the review's, the app forks the next
//!   version pinned to the heads now (fresh and moved sections copied with their citations
//!   remapped), makes it current, and grants the review assistant's session `Refresh` of those
//!   sections on it; a version already pinned to the heads now is refreshed in place. The
//!   assistant is then asked to rewrite just those sections.
//! * **Versions.** `V` lists the guide's versions; picking an older one shows it read-only,
//!   pinned to the code it was written for, until the current one is picked again.
//! * **Since guides.** "Guide these changes" (the Compare menu, the empty guide panel while a
//!   comparison is active, or the palette) creates a guide of what changed since the
//!   comparison's baseline, pinned to the heads now with the compared pairs, grants the
//!   assistant's session `Generate` on its first version, and asks it to write it. A guide
//!   written for the code the baseline compares from is named, so the walkthrough links its
//!   sections. The plugin refuses citations of lines not new since the baseline.

use super::ReviewSurface;
use super::flows::Modal;
use super::guide::Prefer;
use crate::action::Action;
use crate::assistant::LinkRequest;
use crate::context::UiContext;
use crate::guide::GuideKeys;
use crate::loader::Message;
use crate::notes::NoteScope;
use crate::picker::{Picker, PickerItem, PickerOutcome};
use crate::services::ReviewServices;
use bcode_code_review::guide::freshness::GuideFreshness;
use bcode_code_review::guide::lifecycle;
use bcode_code_review_models::{
    CreateReviewGuideRequest, ForkGuideVersionRequest, GetGuideVersionRequest, GrantMode,
    GuideGrant, GuideScope, GuideStatus, GuideVersion, LayerPair, ListGuideVersionsRequest,
    ListReviewGuidesRequest, ResolvedSource,
};
use bmux_keyboard::KeyStroke;
use bmux_tui::event::Event;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::BTreeMap;

/// Guides, and versions of each, read at most when looking for one written for a baseline.
const MAX_LINK_GUIDES: usize = 8;
const MAX_LINK_VERSIONS: usize = 8;

/// A section asked to be refreshed: id, outline number, and title.
pub type RefreshedSection = (String, String, String);
/// A guide named for a since guide to link: id, title, and version.
pub type LinkedGuide = (String, String, u32);

/// Guide lifecycle work that finished off the screen's thread.
#[derive(Debug)]
pub enum LifecycleDone {
    /// A refresh fork was written: the guide, its title, the new version, and the sections to
    /// refresh; or why not.
    Forked(Result<(String, String, u32, Vec<RefreshedSection>), String>),
    /// A since guide was created (its id, and a guide written for the baseline: id, title,
    /// version); or why not.
    SinceStarted(Result<(String, Option<LinkedGuide>), String>),
}

impl ReviewSurface {
    /// The keys the guide's banners name.
    pub(super) fn guide_keys(&self) -> GuideKeys {
        let key = |action: &str| self.action_keys.get(action).cloned().unwrap_or_default();
        GuideKeys {
            refresh: key("guide_refresh"),
            refresh_stale: key("guide_refresh_stale"),
            versions: key("guide_versions"),
        }
    }

    /// Grant the review assistant's session `grant` (none: only the versions it starts); the
    /// session is bound again with it before its next message.
    pub(super) fn set_guide_grant(&mut self, grant: Option<GuideGrant>) {
        if self.guide.grant != grant {
            self.guide.grant = grant;
            self.guide.grant_epoch = self.guide.grant_epoch.wrapping_add(1);
        }
    }

    /// Drop the review's grant once the guide it names has moved past it: another version of
    /// the guide became current, or a `Refresh` has no granted section stale any more. A later
    /// ordinary message then binds the session without it, so it neither writes an older version
    /// nor keeps a narrow grant that refuses starting a guide.
    pub(super) fn expire_guide_grant(&mut self) {
        let (Some(grant), Some(active)) = (&self.guide.grant, self.guide.state.active.as_ref())
        else {
            return;
        };
        if grant.guide_id != active.guide.guide_id {
            return;
        }
        let Some(version) = grant.version else {
            return;
        };
        let superseded = active.guide.current_version != version;
        let refreshed = active.version.version == version
            && matches!(&grant.mode, GrantMode::Refresh { section_ids }
                if !section_ids.iter().any(|id| active.stale.contains(id)));
        if superseded || refreshed {
            self.set_guide_grant(None);
        }
    }

    /// The grant the review gives the assistant's session.
    #[must_use]
    pub const fn guide_grant(&self) -> Option<&GuideGrant> {
        self.guide.grant.as_ref()
    }

    /// The review's sources as they resolve now, each once, primary first.
    pub(super) fn guide_sources(&self) -> Vec<ResolvedSource> {
        let mut sources: Vec<ResolvedSource> = Vec::new();
        if let Some(review) = self.review.as_ref() {
            for group in review.doc.groups() {
                if !sources
                    .iter()
                    .any(|source| source.source_id == group.source.source_id)
                {
                    sources.push(group.source.clone());
                }
            }
        }
        sources
    }

    /// The review's source labels, by source id.
    fn source_labels(&self) -> BTreeMap<String, String> {
        self.workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .sources
                    .iter()
                    .map(|source| (source.id.clone(), source.label.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ----------------------------------------------------------------------------------------
    // Refresh
    // ----------------------------------------------------------------------------------------

    /// `guide_refresh` (the current section) or `guide_refresh_stale` (every stale section):
    /// fork the next version pinned to the heads now, or refresh in place when the version is
    /// already pinned to them, and ask the assistant to rewrite the sections.
    pub(super) fn guide_refresh(&mut self, all: bool, host: &UiContext) -> Action {
        let Some(active) = self.guide.state.active.as_ref() else {
            self.no_guide();
            return Action::Redraw;
        };
        if active.is_old_version() {
            self.set_status(
                format!(
                    "v{} is an older version, kept as it was; refresh the current one (v{})",
                    active.version.version, active.guide.current_version
                ),
                StatusSeverity::Info,
            );
            return Action::Redraw;
        }
        if matches!(
            active.version.status,
            GuideStatus::Planning
                | GuideStatus::Generating
                | GuideStatus::Synthesizing
                | GuideStatus::Verifying
        ) {
            self.set_status(
                "the guide is still being written; refresh it once it is done",
                StatusSeverity::Info,
            );
            return Action::Redraw;
        }
        let sections: Vec<RefreshedSection> = if all {
            active
                .stale_in_order()
                .into_iter()
                .map(|entry| (entry.id.clone(), entry.number.clone(), entry.title.clone()))
                .collect()
        } else {
            active
                .current()
                .filter(|entry| entry.state != bcode_code_review_models::SectionState::Planned)
                .map(|entry| (entry.id.clone(), entry.number.clone(), entry.title.clone()))
                .into_iter()
                .collect()
        };
        if sections.is_empty() {
            self.set_status(
                if all {
                    "no section is stale"
                } else {
                    "pick a written section to refresh"
                },
                StatusSeverity::Info,
            );
            return Action::Redraw;
        }
        let current = self.guide_sources();
        let labels = self.source_labels();
        let heads = lifecycle::heads_of(&current, &labels);
        let (guide_id, title, number) = (
            active.guide.guide_id.clone(),
            active.guide.title.clone(),
            active.version.version,
        );
        if lifecycle::pinned_to(&active.version, &heads) {
            self.ask_refresh(&guide_id, &title, number, &sections);
            return Action::Redraw;
        }
        let freshness = GuideFreshness {
            citations: active.freshness.clone(),
            stale_sections: active
                .stale_in_order()
                .into_iter()
                .map(|entry| entry.id.clone())
                .collect(),
        };
        let (draft, _) = lifecycle::refresh_fork(
            &active.version,
            &freshness,
            &current,
            &labels,
            crate::history::now_ms(),
        );
        let repo_path = self.repo_path.clone();
        let (services, inbox) = (self.services.clone(), self.inbox.clone());
        self.set_status(
            format!("starting v{} of the guide for the new code…", number + 1),
            StatusSeverity::Info,
        );
        host.spawn(Box::pin(async move {
            let result = services
                .call(ForkGuideVersionRequest {
                    repo_path,
                    version: draft,
                    make_current: true,
                })
                .await
                .map(|forked| (guide_id, title, forked.version.version.version, sections))
                .map_err(|error| error.message);
            inbox.push(Message::GuideLifecycle(Box::new(LifecycleDone::Forked(
                result,
            ))));
        }));
        Action::Redraw
    }

    /// Grant the assistant's session `Refresh` of `sections` on `version` of the guide (adding
    /// to a refresh it was already granted there), show that version, and ask it to rewrite
    /// them.
    fn ask_refresh(
        &mut self,
        guide_id: &str,
        title: &str,
        version: u32,
        sections: &[RefreshedSection],
    ) {
        let mut ids: Vec<String> = match &self.guide.grant {
            Some(GuideGrant {
                guide_id: granted,
                version: Some(at),
                mode: GrantMode::Refresh { section_ids },
            }) if granted == guide_id && *at == version => section_ids.clone(),
            _ => Vec::new(),
        };
        for (id, _, _) in sections {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        self.set_guide_grant(Some(GuideGrant {
            guide_id: guide_id.to_owned(),
            version: Some(version),
            mode: GrantMode::Refresh { section_ids: ids },
        }));
        self.guide.view = None;
        self.guide.prefer = Some(Prefer::Guide(guide_id.to_owned()));
        self.reload_guide();
        let named: Vec<String> = sections
            .iter()
            .map(|(id, number, title)| format!("§{number} “{title}” ({id})"))
            .collect();
        let text = format!(
            "Refresh {} of the guide “{title}” (guide {guide_id}, version {version}): the code \
             they cite changed since they were written. Read them with code_review.guide.get \
             (include freshness), then rewrite each with code_review.guide.write put_section on \
             the same id and version {version}, citing the code as it is now with quotes. \
             Change nothing else.",
            named.join(", ")
        );
        self.link
            .requests
            .push(LinkRequest::Message { text, send: true });
        self.set_status(
            format!(
                "asked the assistant to refresh {} section{}",
                sections.len(),
                if sections.len() == 1 { "" } else { "s" }
            ),
            StatusSeverity::Info,
        );
    }

    // ----------------------------------------------------------------------------------------
    // Versions
    // ----------------------------------------------------------------------------------------

    /// `guide_versions`: list the guide's versions, newest first.
    pub(super) fn open_guide_versions(&mut self) -> Action {
        let Some(active) = self.guide.state.active.as_ref() else {
            self.no_guide();
            return Action::Redraw;
        };
        let theme = &self.theme;
        let now = crate::history::now_ms();
        let current = active.guide.current_version;
        let mut items: Vec<PickerItem> = self
            .guide
            .state
            .versions
            .iter()
            .rev()
            .map(|summary| {
                let mut spans = vec![Span::styled(
                    format!("v{}", summary.version),
                    if summary.version == active.version.version {
                        theme.title
                    } else {
                        theme.text
                    },
                )];
                let mut notes = Vec::new();
                if summary.version == current {
                    notes.push("current".to_owned());
                }
                if let Some(status) = &summary.status {
                    notes.push(status.as_str().to_owned());
                }
                if let Some(based_on) = summary.based_on {
                    notes.push(format!("from v{based_on}"));
                }
                notes.push(crate::history::ago(summary.updated_at_ms, now));
                spans.push(Span::styled(
                    format!("{}{}", theme.icons.separator, notes.join(" · ")),
                    theme.muted,
                ));
                PickerItem::new(summary.version.to_string(), Line::from_spans(spans))
            })
            .collect();
        if items.is_empty() {
            items.push(PickerItem::new(
                current.to_string(),
                Line::from_spans(vec![Span::styled(format!("v{current}"), theme.text)]),
            ));
        }
        let mut picker = Picker::new("review.guide.versions", "Guide versions", None);
        picker.set_footer("enter read · esc close · older versions are read-only");
        picker.set_items(items);
        picker.focus_id(&active.version.version.to_string());
        self.modal = Some(Modal::GuideVersions(picker));
        Action::Redraw
    }

    /// Keys while the versions picker is open.
    pub(super) fn guide_versions_key(&mut self, stroke: KeyStroke) -> Action {
        let Some(Modal::GuideVersions(picker)) = self.modal.as_mut() else {
            return Action::None;
        };
        let outcome = picker.key(stroke);
        self.guide_versions_outcome(outcome)
    }

    /// Pointer events while the versions picker is open.
    pub(super) fn guide_versions_mouse(&mut self, event: &Event) -> Action {
        let theme = self.theme;
        let Some(Modal::GuideVersions(picker)) = self.modal.as_mut() else {
            return Action::None;
        };
        let outcome = picker.mouse(event, &theme);
        self.guide_versions_outcome(outcome)
    }

    fn guide_versions_outcome(&mut self, outcome: PickerOutcome) -> Action {
        match outcome {
            PickerOutcome::Picked(id) => {
                self.modal = None;
                if let Ok(version) = id.parse() {
                    self.show_guide_version(version);
                }
                Action::Redraw
            }
            PickerOutcome::Cancel => {
                self.modal = None;
                Action::Redraw
            }
            PickerOutcome::Ignored => Action::None,
            _ => Action::Redraw,
        }
    }

    /// Show `version` of the guide on screen (its current version shows as usual).
    pub(super) fn show_guide_version(&mut self, version: u32) {
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let guide_id = active.guide.guide_id.clone();
        self.guide.view =
            (version != active.guide.current_version).then(|| (guide_id.clone(), version));
        self.guide.prefer = Some(Prefer::Guide(guide_id));
        self.guide.state.engaged = true;
        self.reload_guide();
    }

    /// Ask for `version` of guide `guide_id` (a `guide_show` call naming one): shown once read.
    pub(super) fn want_guide_version(&mut self, guide_id: &str, version: u32) {
        let current = self
            .guide
            .state
            .guides
            .iter()
            .find(|record| record.guide.guide_id == guide_id)
            .map(|record| record.guide.current_version);
        self.guide.view = (current != Some(version)).then(|| (guide_id.to_owned(), version));
        self.guide.prefer = Some(Prefer::Guide(guide_id.to_owned()));
        self.reload_guide();
    }

    // ----------------------------------------------------------------------------------------
    // Since guides
    // ----------------------------------------------------------------------------------------

    /// `guide_since`: guide the changes since the comparison's baseline.
    pub(super) fn guide_since(&mut self, host: &UiContext) -> Action {
        let pairs: Vec<LayerPair> = self
            .comparing
            .current_pairs()
            .into_iter()
            .map(|(_, pair)| pair)
            .collect();
        if pairs.is_empty() {
            self.set_status(
                "nothing is compared yet: pick what to compare with (Compare menu), then guide \
                 those changes",
                StatusSeverity::Info,
            );
            return Action::Redraw;
        }
        let baseline = self.comparing.baseline();
        let label = self.baseline_labels()[0].clone();
        let current = self.guide_sources();
        let labels = self.source_labels();
        let workspace_id = self
            .workspace()
            .map(bcode_code_review::scope::annotation_workspace_id)
            .unwrap_or_default();
        let (guide, version) = lifecycle::since_guide(
            &workspace_id,
            GuideScope::Since {
                baseline,
                pairs: pairs.clone(),
            },
            &format!("Changes since {label}"),
            (&current, &labels),
            Some(format!("walk through what changed since {label}")),
            crate::history::now_ms(),
        );
        let scope = self.note_scope();
        let (services, inbox) = (self.services.clone(), self.inbox.clone());
        self.set_status("starting a guide of the changes…", StatusSeverity::Info);
        host.spawn(Box::pin(async move {
            let result = start_since(&services, scope, guide, version, &pairs).await;
            inbox.push(Message::GuideLifecycle(Box::new(
                LifecycleDone::SinceStarted(result),
            )));
        }));
        Action::Redraw
    }

    /// Apply finished lifecycle work.
    pub(super) fn apply_guide_lifecycle(&mut self, done: LifecycleDone) {
        match done {
            LifecycleDone::Forked(Ok((guide_id, title, version, sections))) => {
                self.ask_refresh(&guide_id, &title, version, &sections);
            }
            LifecycleDone::Forked(Err(error)) => self.set_status(
                format!("could not start a new version of the guide: {error}"),
                StatusSeverity::Warning,
            ),
            LifecycleDone::SinceStarted(Ok((guide_id, earlier))) => {
                self.set_guide_grant(Some(GuideGrant {
                    guide_id: guide_id.clone(),
                    version: Some(1),
                    mode: GrantMode::Generate,
                }));
                self.guide.view = None;
                self.guide.prefer = Some(Prefer::Guide(guide_id.clone()));
                self.guide.state.engaged = true;
                if self.panels.guide_fits() && !self.panels.guide_open {
                    self.panels.show_guide();
                    self.persist_panels();
                }
                self.reload_guide();
                let link = earlier.map_or_else(String::new, |(id, title, version)| {
                    format!(
                        " An earlier guide, “{title}” (guide {id}, version {version}), was \
                         written for the code the baseline compares from: read it with \
                         code_review.guide.get and open with what changed in its sections, \
                         naming them (§ numbers of its version {version})."
                    )
                });
                let text = format!(
                    "Walk me through what changed since I last looked. Guide {guide_id} (version \
                     1) is started for this: plan its outline with code_review.guide.write \
                     set_outline, write each section with put_section, then set_status complete; \
                     do not call code_review.guide.start. Cite only lines new since the \
                     comparison baseline.{link}"
                );
                self.link
                    .requests
                    .push(LinkRequest::Message { text, send: true });
                self.set_status(
                    "asked the assistant to walk you through the changes",
                    StatusSeverity::Info,
                );
            }
            LifecycleDone::SinceStarted(Err(error)) => self.set_status(
                format!("could not start a guide of the changes: {error}"),
                StatusSeverity::Warning,
            ),
        }
    }
}

/// Create the since guide, and look for a guide written for the code the baseline compares
/// from (bounded reads).
///
/// # Errors
///
/// * Creating the guide failed.
async fn start_since(
    services: &ReviewServices,
    scope: NoteScope,
    guide: bcode_code_review_models::ReviewGuide,
    version: GuideVersion,
    pairs: &[LayerPair],
) -> Result<(String, Option<LinkedGuide>), String> {
    let earlier = earlier_guide(services, &scope, pairs).await;
    let created = services
        .call(CreateReviewGuideRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            guide,
            version,
        })
        .await
        .map_err(|error| error.message)?;
    Ok((created.guide.guide.guide_id, earlier))
}

/// The newest version of the review's guides (other than since guides) pinned to a head the
/// baseline compares from, when one is found within the read bounds.
async fn earlier_guide(
    services: &ReviewServices,
    scope: &NoteScope,
    pairs: &[LayerPair],
) -> Option<LinkedGuide> {
    let guides = services
        .call(ListReviewGuidesRequest {
            repo_path: scope.repo_path.clone(),
            target: scope.target.clone(),
            scope: scope.scope.clone(),
            include_archived: false,
        })
        .await
        .ok()?
        .guides;
    for record in guides
        .iter()
        .filter(|record| !matches!(record.guide.scope, GuideScope::Since { .. }))
        .take(MAX_LINK_GUIDES)
    {
        let Ok(listed) = services
            .call(ListGuideVersionsRequest {
                repo_path: scope.repo_path.clone(),
                guide_id: record.guide.guide_id.clone(),
            })
            .await
        else {
            continue;
        };
        for summary in listed.versions.iter().rev().take(MAX_LINK_VERSIONS) {
            let Ok(read) = services
                .call(GetGuideVersionRequest {
                    repo_path: scope.repo_path.clone(),
                    guide_id: record.guide.guide_id.clone(),
                    version: Some(summary.version),
                })
                .await
            else {
                continue;
            };
            if read
                .version
                .is_some_and(|read| lifecycle::written_for_baseline(&read.version, pairs))
            {
                return Some((
                    record.guide.guide_id.clone(),
                    record.guide.title.clone(),
                    summary.version,
                ));
            }
        }
    }
    None
}
