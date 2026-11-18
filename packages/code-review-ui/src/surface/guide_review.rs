//! Reviewing with the guide: reviewed sections and viewed files, findings to draft comments,
//! expanding, asking about, promoting and discarding under a section, and restructuring.
//!
//! * **Reviewed** (`v`). Marks the current section with its citations' fingerprint and marks
//!   files viewed through the review's ordinary viewed path, by `guide.mark_files_viewed`
//!   ([`bcode_code_review::guide::coverage`]); a section also counts as reviewed when every
//!   in-diff file it cites is viewed. Un-viewing a file un-reviews the sections citing it.
//! * **Finding to comment** (`c`). Opens the comment composer on the finding's first in-diff
//!   citation, prefilled with its text, kind and severity; once the draft is saved its thread is
//!   recorded on the finding.
//! * **Expand** (`e`). Sends "Expand §n" to the assistant's conversation, bound for that
//!   message with an `Expand` grant on the section, with the section as a context block. What
//!   it adds is a provisional subsection.
//! * **Ask** (`a`). The question composer; the question goes to a session of its own bound with
//!   an `Ask` grant on the section (follow-ups to the same session), recorded as a guide thread
//!   whose answer shows under the section, also after reopening.
//! * **Promote** (`p`) and **discard** (`x`). A provisional section is made written or removed
//!   by the app; an ask thread is promoted by asking its session to write its answer as an
//!   expansion and promote it, or discarded (hidden).
//! * **Restructure** (`shift+r`). The composer asks for directions; the app forks the next
//!   version (based on the one shown, which is kept) and sends the directions to the assistant
//!   bound with a `Restructure` grant on that new version.

use super::guide::Prefer;
use super::{Focus, ReviewSurface};
use crate::assistant::LinkRequest;
use crate::composer::ComposerMode;
use crate::context::UiContext;
use crate::guide::panel::{DetailLine, MarkdownBody};
use crate::loader::Message;
use crate::notes::NoteScope;
use crate::services::ReviewServices;
use bcode_code_review::document::DocAnchor;
use bcode_code_review::guide::config::MarkFilesViewed;
use bcode_code_review::guide::coverage::{self, ReviewFile};
use bcode_code_review::guide::write::{GuideOp, Writer};
use bcode_code_review_models::{
    ClearGuideSectionMarkRequest, FindingKind, ForkGuideVersionRequest, GetGuideVersionRequest,
    GrantMode, GuideGrant, GuideSectionMark, GuideStatus, GuideThread, GuideThreadStatus,
    GuideVersion, ResolvedSource, ReviewThreadKind, SaveGuideThreadRequest,
    SaveGuideVersionRequest, SectionState, SetGuideSectionMarkRequest,
};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::status_bar::StatusSeverity;
use std::collections::{BTreeMap, BTreeSet};

/// Attempts at an app write of a version when its revision keeps changing.
const WRITE_ATTEMPTS: usize = 4;
/// Label of the context block a guide action sends.
const SECTION_BLOCK: &str = "guide section";

/// A finding of a guide version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FindingRef {
    guide_id: String,
    version: u32,
    finding_id: String,
}

/// What a composer opened from the guide is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Composing {
    /// A draft comment from a finding.
    Comment(FindingRef),
    /// A question about a section.
    Ask {
        guide_id: String,
        version: u32,
        section_id: String,
    },
    /// Directions for restructuring a version.
    Restructure { guide_id: String, version: u32 },
}

/// The guide's review actions' state on screen.
#[derive(Debug, Default)]
pub(super) struct GuideReviewState {
    /// The composer opened from the guide, with the anchor it was opened at.
    composing: Option<(DocAnchor, Composing)>,
    /// Draft comments from findings waiting to be saved: the finding and the draft's local id.
    drafted: Vec<(FindingRef, String)>,
    /// The guide and the in-diff files it cites the review showed viewed, as last seen.
    viewed: Option<(String, BTreeSet<ReviewFile>)>,
    /// What the viewed files were last computed from.
    viewed_stamp: Option<(u64, u64)>,
    /// Questions and directions submitted from guide composers, to send.
    submitted: Vec<(Composing, String)>,
}

/// A guide review action that finished in the background.
#[derive(Debug)]
pub enum GuideReviewDone {
    /// Files a reviewed section marks viewed.
    ViewFiles(BTreeSet<ReviewFile>),
    /// A guide write finished: a status to show, or why it failed.
    Written(Result<String, String>),
    /// The version to restructure was forked.
    Forked {
        /// The guide.
        guide_id: String,
        /// The new version.
        version: u32,
        /// The reviewer's directions.
        directions: String,
    },
}

impl ReviewSurface {
    /// A guide panel key for a review action; `None` for other actions.
    pub(super) fn guide_review_key(&mut self, action: &str, host: &UiContext) -> Option<()> {
        match action {
            "guide_reviewed" => self.guide_toggle_reviewed(host),
            "guide_comment" => self.guide_comment_from_finding(host),
            "guide_expand" => self.guide_expand(),
            "guide_ask" => self.guide_open_composer(true),
            "guide_restructure" => self.guide_open_composer(false),
            "guide_promote" => self.guide_promote(host),
            "guide_discard" => self.guide_discard(host),
            "guide_next_unreviewed" => {
                self.guide_next_unreviewed(host);
            }
            _ => return None,
        }
        Some(())
    }

    /// The current section's id and number, the guide and the version, when the version
    /// shown is the current one (older versions are read-only).
    fn guide_current(&mut self) -> Option<(String, String, String, u32)> {
        let active = self.guide.state.active.as_ref()?;
        let entry = active.current()?;
        let found = (
            entry.id.clone(),
            entry.number.clone(),
            active.guide.guide_id.clone(),
            active.version.version,
        );
        if active.is_old_version() {
            self.set_status(
                "this version is kept read-only; open the current one to change it",
                StatusSeverity::Info,
            );
            return None;
        }
        Some(found)
    }

    // ----------------------------------------------------------------------------------------
    // Reviewed and viewed
    // ----------------------------------------------------------------------------------------

    /// `v`: mark the current section reviewed (marking files viewed), or un-review it.
    fn guide_toggle_reviewed(&mut self, host: &UiContext) {
        let Some((id, number, guide_id, _)) = self.guide_current() else {
            return;
        };
        let scope = self.note_scope();
        let Some(active) = self.guide.state.active.as_mut() else {
            return;
        };
        if active.reviewed(&id) {
            let was_marked = active.marks.remove(&id).is_some();
            let still = active.reviewed(&id);
            let files = coverage::cited_files(&active.version, &id);
            self.guide.state.bump();
            if was_marked {
                let services = self.services.clone();
                let (repo, guide, section) = (scope.repo_path, guide_id, id.clone());
                host.spawn(Box::pin(async move {
                    drop(
                        services
                            .call(ClearGuideSectionMarkRequest {
                                repo_path: repo,
                                guide_id: guide,
                                section_id: section,
                            })
                            .await,
                    );
                }));
            }
            if still {
                // Reviewed because its files are viewed. The guide changes viewed files only
                // when it may mark them (`mark_files_viewed` other than `never`); then
                // un-viewing them un-reviews it.
                if self.services.guide_config().mark_files_viewed == MarkFilesViewed::Never {
                    self.set_status(
                        format!(
                            "§{number} counts as reviewed while its files are viewed; un-view \
                             them to un-review it"
                        ),
                        StatusSeverity::Info,
                    );
                    return;
                }
                self.set_files_viewed(&files, false);
            }
            self.set_status(format!("§{number} not reviewed"), StatusSeverity::Info);
            return;
        }
        let Some(section) = active.section(&id) else {
            return;
        };
        let mark = GuideSectionMark {
            guide_id,
            section_id: id.clone(),
            reviewed_at_ms: crate::assistant::panel::now_ms(),
            fingerprint: bcode_code_review::guide::document::section_fingerprint(
                section,
                &active.version.citations,
            ),
        };
        active.marks.insert(id.clone(), mark.clone());
        let reviewed: BTreeSet<String> = active
            .entries
            .iter()
            .filter(|entry| active.marked(&entry.id))
            .map(|entry| entry.id.clone())
            .collect();
        let version = active.version.clone();
        self.guide.state.bump();
        let mode = self.services.guide_config().mark_files_viewed;
        let services = self.services.clone();
        let sources = self.guide_sources();
        let inbox = self.inbox.clone();
        let repo = scope.repo_path;
        host.spawn(Box::pin(async move {
            let saved = services
                .call(SetGuideSectionMarkRequest {
                    repo_path: repo.clone(),
                    mark,
                })
                .await;
            if let Err(error) = saved {
                inbox.push(guide_done(GuideReviewDone::Written(Err(error.message))));
                return;
            }
            if mode == MarkFilesViewed::Never {
                return;
            }
            let data = services.data();
            let files = services
                .blocking(move || {
                    Ok(files_to_view(
                        &data, repo, &sources, &version, &reviewed, mode,
                    ))
                })
                .await
                .unwrap_or_default();
            if !files.is_empty() {
                inbox.push(guide_done(GuideReviewDone::ViewFiles(files)));
            }
        }));
        self.set_status(format!("§{number} reviewed"), StatusSeverity::Success);
    }

    /// Mark `files` viewed (or unviewed) through the review's viewed path.
    fn set_files_viewed(&mut self, files: &BTreeSet<ReviewFile>, viewed: bool) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let identities: Vec<_> = review
            .doc
            .file_keys()
            .filter(|file| files.contains(&(file.source_id.clone(), file.path.clone())))
            .filter(|file| {
                review
                    .doc
                    .viewed_state(file)
                    .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
                    != viewed
            })
            .filter_map(|file| review.doc.viewed_identity(file))
            .collect();
        if identities.is_empty() {
            return;
        }
        let workspace = self.workspace_mut();
        for (identity, patch) in &identities {
            let file = identity.with_patch(patch.as_deref());
            if viewed {
                bcode_code_review::viewed::mark_viewed(workspace, &file);
            } else {
                bcode_code_review::viewed::unmark_viewed(workspace, &file);
            }
        }
        self.apply_viewed();
    }

    /// Follow the review's viewed files: which in-diff files the guide cites are viewed, and
    /// un-review the marked sections citing a file that was un-viewed. Also forget a guide
    /// composer that closed and link saved drafts to their findings.
    pub(super) fn sync_guide_review(&mut self, host: &UiContext) {
        if self.composer.is_none() {
            self.guide.review.composing = None;
        }
        self.link_drafted_findings(host);
        self.send_composed(host);
        let (Some(review), Some(active)) = (self.review.as_ref(), self.guide.state.active.as_ref())
        else {
            return;
        };
        let stamp = (self.guide.state.revision, review.doc.revision());
        if self.guide.review.viewed_stamp == Some(stamp) {
            return;
        }
        self.guide.review.viewed_stamp = Some(stamp);
        let cited: BTreeSet<ReviewFile> = active
            .entries
            .iter()
            .flat_map(|entry| coverage::cited_files(&active.version, &entry.id))
            .collect();
        let viewed: BTreeSet<ReviewFile> = review
            .doc
            .file_keys()
            .filter(|file| {
                cited.contains(&(file.source_id.clone(), file.path.clone()))
                    && review
                        .doc
                        .viewed_state(file)
                        .is_some_and(bcode_code_review::viewed::ViewedState::is_viewed)
            })
            .map(|file| (file.source_id.clone(), file.path.clone()))
            .collect();
        let guide_id = active.guide.guide_id.clone();
        let unviewed: BTreeSet<ReviewFile> = match &self.guide.review.viewed {
            Some((known, before)) if *known == guide_id => {
                before.difference(&viewed).cloned().collect()
            }
            _ => BTreeSet::new(),
        };
        self.guide.review.viewed = Some((guide_id.clone(), viewed.clone()));
        let mode = self.services.guide_config().mark_files_viewed;
        let clear: Vec<String> = if mode == MarkFilesViewed::Never || unviewed.is_empty() {
            Vec::new()
        } else {
            coverage::sections_citing(&active.version, &unviewed)
                .into_iter()
                .filter(|id| active.marks.contains_key(id))
                .collect()
        };
        let changed = active.viewed != viewed || !clear.is_empty();
        let scope = self.note_scope();
        let Some(active) = self.guide.state.active.as_mut() else {
            return;
        };
        active.viewed = viewed;
        for id in &clear {
            active.marks.remove(id);
        }
        if changed {
            self.guide.state.bump();
        }
        if clear.is_empty() {
            return;
        }
        let services = self.services.clone();
        host.spawn(Box::pin(async move {
            for section in clear {
                drop(
                    services
                        .call(ClearGuideSectionMarkRequest {
                            repo_path: scope.repo_path.clone(),
                            guide_id: guide_id.clone(),
                            section_id: section,
                        })
                        .await,
                );
            }
        }));
    }

    // ----------------------------------------------------------------------------------------
    // Findings to draft comments
    // ----------------------------------------------------------------------------------------

    /// `c`: open the comment composer on the current section's first finding not drafted yet.
    fn guide_comment_from_finding(&mut self, host: &UiContext) {
        let Some((id, _, guide_id, version)) = self.guide_current() else {
            return;
        };
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let Some(section) = active.section(&id) else {
            return;
        };
        let Some(finding) = section
            .findings
            .iter()
            .find(|finding| finding.drafted_thread_id.is_none())
            .or_else(|| section.findings.first())
            .cloned()
        else {
            self.set_status("this section has no findings", StatusSeverity::Info);
            return;
        };
        let cite = finding
            .citations
            .iter()
            .find(|cited| {
                active.citation(cited).is_some_and(|citation| {
                    citation.relation != bcode_code_review_models::CitationRelation::OutsideReview
                })
            })
            .and_then(|cited| {
                active
                    .current()
                    .and_then(|entry| entry.citations.iter().position(|known| known == cited))
            });
        let selected = active.selected;
        if cite.is_some() {
            self.guide_show(selected, cite, host);
        }
        let anchor = if cite.is_some() && self.guide.peek.is_none() {
            self.cursor_anchor().unwrap_or_else(DocAnchor::review)
        } else {
            DocAnchor::review()
        };
        let kind = match finding.kind {
            FindingKind::Risk | FindingKind::Suggestion => ReviewThreadKind::Finding,
            FindingKind::Question => ReviewThreadKind::Question,
            FindingKind::Note => ReviewThreadKind::Note,
        };
        self.open_composer(anchor.clone(), ComposerMode::Comment, &finding.text);
        if let Some(composer) = self.composer.as_mut() {
            composer.kind = kind;
            if let Some(severity) = finding.severity {
                composer.severity = severity;
            }
        }
        self.guide.review.composing = Some((
            anchor,
            Composing::Comment(FindingRef {
                guide_id,
                version,
                finding_id: finding.id,
            }),
        ));
    }

    /// A comment composer was submitted as the draft `local` at `anchor`: when it was opened
    /// from a finding, link the draft to it once saved.
    pub(super) fn guide_comment_drafted(&mut self, anchor: &DocAnchor, local: String) {
        if let Some((at, Composing::Comment(finding))) = self.guide.review.composing.take()
            && at == *anchor
        {
            self.guide.review.drafted.push((finding, local));
        }
    }

    /// Record the thread of each saved draft on its finding.
    fn link_drafted_findings(&mut self, host: &UiContext) {
        if self.guide.review.drafted.is_empty() {
            return;
        }
        let mut linked = Vec::new();
        self.guide.review.drafted.retain(|(finding, local)| {
            self.notes.saved_thread(local).is_none_or(|thread| {
                linked.push((finding.clone(), thread.to_owned()));
                false
            })
        });
        for (finding, thread) in linked {
            let id = finding.finding_id.clone();
            self.guide_edit(
                host,
                &finding.guide_id,
                finding.version,
                "drafted the finding as a comment",
                move |version| {
                    let found = find_finding(&mut version.outline, &id)
                        .ok_or_else(|| format!("the guide has no finding {id}"))?;
                    found.drafted_thread_id = Some(thread.clone());
                    Ok(())
                },
            );
        }
    }

    // ----------------------------------------------------------------------------------------
    // Expand, ask, promote, discard, restructure
    // ----------------------------------------------------------------------------------------

    /// The section `id` of the version shown as a context block, with what the grant allows.
    fn section_block(&self, id: &str, allowed: &str) -> Vec<crate::host::ContextBlock> {
        let Some(active) = self.guide.state.active.as_ref() else {
            return Vec::new();
        };
        let markdown = bcode_code_review::guide::export::render_markdown(
            &active.version,
            &bcode_code_review::guide::export::ExportOptions {
                title: &active.guide.title,
                links: bcode_code_review::guide::export::LinkStyle::Relative,
                include_unverified: true,
                sections: vec![id.to_owned()],
                ..Default::default()
            },
        );
        vec![crate::host::ContextBlock {
            label: SECTION_BLOCK.to_owned(),
            text: format!(
                "Guide {} v{}, section {id}. {allowed}\n\n{markdown}",
                active.guide.guide_id, active.version.version
            ),
            request_only: false,
        }]
    }

    /// `e`: ask the assistant to expand the current section, bound to add only under it.
    fn guide_expand(&mut self) {
        let Some((id, number, guide_id, version)) = self.guide_current() else {
            return;
        };
        let title = self
            .guide
            .state
            .active
            .as_ref()
            .and_then(|active| active.current().map(|entry| entry.title.clone()))
            .unwrap_or_default();
        let context = self.section_block(
            &id,
            &format!(
                "Expand it: write one code_review.guide.write with add_expansion under {id} \
                 (cited, with quotes). Nothing else in the guide may change."
            ),
        );
        self.link.requests.push(LinkRequest::GuideMessage {
            text: format!("Expand §{number} {title}."),
            grant: Some(GuideGrant {
                guide_id,
                version: Some(version),
                mode: GrantMode::Expand { section_id: id },
            }),
            context,
            ticket: None,
        });
        self.set_status(
            format!("asked the assistant to expand §{number}"),
            StatusSeverity::Info,
        );
    }

    /// `a` (`ask`) or `shift+r`: the composer for a question about the current section, or for
    /// directions to restructure the guide.
    fn guide_open_composer(&mut self, ask: bool) {
        let Some((id, number, guide_id, version)) = self.guide_current() else {
            return;
        };
        let anchor = self.cursor_anchor().unwrap_or_else(DocAnchor::review);
        let (text, composing, status) = if ask {
            (
                String::new(),
                Composing::Ask {
                    guide_id,
                    version,
                    section_id: id,
                },
                format!("ask about §{number}; enter sends"),
            )
        } else {
            (
                "Restructure the guide: ".to_owned(),
                Composing::Restructure { guide_id, version },
                format!("restructuring writes v{} and keeps v{version}", version + 1),
            )
        };
        self.open_composer(anchor.clone(), ComposerMode::Ask, &text);
        self.guide.review.composing = Some((anchor, composing));
        self.set_status(status, StatusSeverity::Info);
    }

    /// A question composer was submitted with `text` at `anchor`: when it was opened from the
    /// guide, do what it was for and say so. `None` for an ordinary question.
    pub(super) fn guide_composed(
        &mut self,
        anchor: &DocAnchor,
        text: String,
    ) -> Option<&'static str> {
        let (at, composing) = self.guide.review.composing.take()?;
        if at != *anchor {
            return None;
        }
        let status = match composing {
            Composing::Comment(_) => return None,
            Composing::Ask { .. } => "asking about the section",
            Composing::Restructure { .. } => "restructuring the guide as a new version",
        };
        self.guide.review.submitted.push((composing, text));
        Some(status)
    }

    /// Send what guide composers submitted.
    fn send_composed(&mut self, host: &UiContext) {
        for (composing, text) in std::mem::take(&mut self.guide.review.submitted) {
            match composing {
                Composing::Comment(_) => {}
                Composing::Ask {
                    guide_id,
                    version,
                    section_id,
                } => self.guide_ask(host, &guide_id, version, &section_id, text),
                Composing::Restructure { guide_id, version } => {
                    let directions = text
                        .trim()
                        .trim_start_matches("Restructure the guide:")
                        .trim()
                        .to_owned();
                    self.guide_restructure(host, &guide_id, version, directions);
                }
            }
        }
    }

    /// Ask `question` about `section_id`: in the section's open ask session, else a new one
    /// bound with an `Ask` grant; recorded as a guide thread.
    fn guide_ask(
        &mut self,
        host: &UiContext,
        guide_id: &str,
        version: u32,
        section_id: &str,
        question: String,
    ) {
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let session = active
            .open_threads(section_id)
            .filter_map(|thread| thread.session_id.clone())
            .last();
        let (number, title) = active
            .entries
            .iter()
            .find(|entry| entry.id == section_id)
            .map(|entry| (entry.number.clone(), entry.title.clone()))
            .unwrap_or_default();
        let now = crate::assistant::panel::now_ms();
        let thread = GuideThread {
            thread_id: format!("gt-{now}-{}", active.threads.len() + 1),
            guide_id: guide_id.to_owned(),
            version,
            section_id: section_id.to_owned(),
            session_id: session.clone(),
            status: GuideThreadStatus::Open,
            question: question.clone(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        let block = self
            .section_block(
                section_id,
                &format!(
                    "Answer the reviewer's question about it, citing code. If the answer belongs \
                     in the guide, you may add it with code_review.guide.write add_expansion \
                     under {section_id}; nothing else in the guide may change."
                ),
            )
            .into_iter()
            .map(|block| block.text)
            .collect::<String>();
        let prompt = if session.is_some() {
            question
        } else {
            format!("{block}\n\nQuestion: {question}")
        };
        if let Some(active) = self.guide.state.active.as_mut() {
            active.threads.push(thread.clone());
        }
        self.guide.state.bump();
        let mut binding = self.assistant_binding(String::new());
        binding.guide_grant = Some(GuideGrant {
            guide_id: guide_id.to_owned(),
            version: Some(version),
            mode: GrantMode::Ask {
                section_id: section_id.to_owned(),
            },
        });
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let workspace = binding.workspace.id.clone();
        let title = format!("Guide: §{number} {title}");
        host.spawn(Box::pin(async move {
            let result = ask_in_session(
                &services,
                &scope,
                thread,
                binding,
                (title, prompt, workspace),
            )
            .await
            .map(|()| "asked about the section".to_owned());
            inbox.push(guide_done(GuideReviewDone::Written(result)));
        }));
    }

    /// `p`: promote the current provisional section, else the section's latest open answer.
    fn guide_promote(&mut self, host: &UiContext) {
        let Some((id, number, guide_id, version)) = self.guide_current() else {
            return;
        };
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        if active
            .current()
            .is_some_and(|entry| matches!(entry.state, SectionState::Provisional { .. }))
        {
            self.guide_apply(host, &guide_id, version, GuideOp::Promote { section: id });
            self.set_status(format!("§{number} promoted"), StatusSeverity::Success);
            return;
        }
        let Some(thread) = active.open_threads(&id).last().cloned() else {
            self.set_status(
                "nothing provisional here: promote an expansion or an answer",
                StatusSeverity::Info,
            );
            return;
        };
        let Some(session) = thread.session_id.clone() else {
            self.set_status("the answer is not ready yet", StatusSeverity::Info);
            return;
        };
        let text = format!(
            "Write your answer to \"{}\" into the guide: one code_review.guide.write with \
             add_expansion under {id} (cited, with quotes), then promote that expansion.",
            thread.question
        );
        self.save_thread_status(
            host,
            thread,
            GuideThreadStatus::Promoted,
            Some((session, text)),
        );
        self.set_status(
            format!("asked to write the answer into §{number}"),
            StatusSeverity::Info,
        );
    }

    /// `x`: discard the current provisional section, else hide the section's latest open
    /// answer.
    fn guide_discard(&mut self, host: &UiContext) {
        let Some((id, number, guide_id, version)) = self.guide_current() else {
            return;
        };
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        if active
            .current()
            .is_some_and(|entry| matches!(entry.state, SectionState::Provisional { .. }))
        {
            self.guide_apply(host, &guide_id, version, GuideOp::Discard { section: id });
            self.set_status(format!("§{number} discarded"), StatusSeverity::Info);
            return;
        }
        let Some(thread) = active.open_threads(&id).last().cloned() else {
            self.set_status(
                "nothing provisional here: discard an expansion or an answer",
                StatusSeverity::Info,
            );
            return;
        };
        self.save_thread_status(host, thread, GuideThreadStatus::Discarded, None);
        self.set_status("answer hidden", StatusSeverity::Info);
    }

    /// Set `thread`'s status (shown at once), then send `message` to its session.
    fn save_thread_status(
        &mut self,
        host: &UiContext,
        mut thread: GuideThread,
        status: GuideThreadStatus,
        message: Option<(String, String)>,
    ) {
        thread.status = status;
        thread.updated_at_ms = crate::assistant::panel::now_ms();
        if let Some(active) = self.guide.state.active.as_mut()
            && let Some(known) = active
                .threads
                .iter_mut()
                .find(|known| known.thread_id == thread.thread_id)
        {
            known.clone_from(&thread);
        }
        self.guide.state.bump();
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let result = async {
                services
                    .call(SaveGuideThreadRequest {
                        repo_path: scope.repo_path.clone(),
                        thread,
                    })
                    .await
                    .map_err(|error| error.message)?;
                if let Some((session, text)) = message {
                    services
                        .host()
                        .send_message(session, text)
                        .await
                        .map_err(|error| error.message)?;
                }
                Ok(String::new())
            }
            .await;
            inbox.push(guide_done(GuideReviewDone::Written(result)));
        }));
    }

    /// Apply `op` to version `version` of `guide_id` as the reviewer.
    fn guide_apply(&mut self, host: &UiContext, guide_id: &str, version: u32, op: GuideOp) {
        self.guide_edit(host, guide_id, version, "", move |doc| {
            let writer = Writer {
                mode: &GrantMode::Restructure,
                session_id: None,
                now_ms: crate::assistant::panel::now_ms(),
            };
            bcode_code_review::guide::write::apply(doc, std::slice::from_ref(&op), &[], &writer)
                .map(drop)
                .map_err(|errors| {
                    errors
                        .into_iter()
                        .map(|error| error.message)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
        });
    }

    /// Change version `version` of `guide_id` with `change`, revision-checked, then read the
    /// guide again; `done` is the status shown once written.
    fn guide_edit(
        &mut self,
        host: &UiContext,
        guide_id: &str,
        version: u32,
        done: &'static str,
        change: impl Fn(&mut GuideVersion) -> Result<(), String> + Send + 'static,
    ) {
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let guide_id = guide_id.to_owned();
        host.spawn(Box::pin(async move {
            let result = edit_version(&services, &scope, &guide_id, version, change)
                .await
                .map(|()| done.to_owned());
            inbox.push(guide_done(GuideReviewDone::Written(result)));
        }));
    }

    /// Fork the next version of `guide_id` from `version` (kept as it is) and ask the assistant
    /// to restructure it.
    fn guide_restructure(
        &mut self,
        host: &UiContext,
        guide_id: &str,
        version: u32,
        directions: String,
    ) {
        let scope = self.note_scope();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let guide_id = guide_id.to_owned();
        host.spawn(Box::pin(async move {
            let result = fork_version(&services, &scope, &guide_id, version, &directions).await;
            inbox.push(guide_done(match result {
                Ok(forked) => GuideReviewDone::Forked {
                    guide_id,
                    version: forked,
                    directions,
                },
                Err(error) => GuideReviewDone::Written(Err(error)),
            }));
        }));
    }

    /// A guide review action finished in the background.
    pub(super) fn guide_review_done(&mut self, done: GuideReviewDone) {
        match done {
            GuideReviewDone::ViewFiles(files) => self.set_files_viewed(&files, true),
            GuideReviewDone::Written(Ok(status)) => {
                if !status.is_empty() {
                    self.set_status(status, StatusSeverity::Success);
                }
                self.reload_guide();
            }
            GuideReviewDone::Written(Err(error)) => {
                self.set_status(
                    format!("the guide was not changed: {error}"),
                    StatusSeverity::Warning,
                );
                self.reload_guide();
            }
            GuideReviewDone::Forked {
                guide_id,
                version,
                directions,
            } => {
                self.guide.prefer = Some(Prefer::Guide(guide_id.clone()));
                self.reload_guide();
                let directions = if directions.is_empty() {
                    "reorganize it so it reads best".to_owned()
                } else {
                    directions
                };
                self.link.requests.push(LinkRequest::GuideMessage {
                    text: format!("Restructure the guide (v{version}): {directions}"),
                    grant: Some(GuideGrant {
                        guide_id: guide_id.clone(),
                        version: Some(version),
                        mode: GrantMode::Restructure,
                    }),
                    context: vec![crate::host::ContextBlock {
                        label: SECTION_BLOCK.to_owned(),
                        text: format!(
                            "Guide {guide_id} v{version} is a copy of v{} to restructure: write \
                             it with code_review.guide.write (version {version}; set_outline, \
                             put_section, move_section, remove_section as needed), then \
                             set_status complete. v{} stays as it is.",
                            version.saturating_sub(1),
                            version.saturating_sub(1)
                        ),
                        request_only: false,
                    }],
                    ticket: None,
                });
                self.focus = Focus::Review;
            }
        }
    }

    /// Sessions of the guide's ask threads, for following their answers.
    pub(super) fn guide_thread_sessions(&self) -> Vec<String> {
        self.guide
            .state
            .active
            .as_ref()
            .map(|active| {
                active
                    .threads
                    .iter()
                    .filter(|thread| thread.status == GuideThreadStatus::Open)
                    .filter_map(|thread| thread.session_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A revision of what the current section's answers show.
    pub(super) fn guide_threads_revision(&self) -> u64 {
        let Some(active) = self.guide.state.active.as_ref() else {
            return 0;
        };
        let Some(entry) = active.current() else {
            return 0;
        };
        active
            .open_threads(&entry.id)
            .filter_map(|thread| thread.session_id.as_deref())
            .fold(0, |revision, session| {
                revision ^ self.agents.session_revision(session).rotate_left(7)
            })
    }

    /// The current section's provisional note and its ask threads (question and answer), as
    /// lines under the section.
    pub(super) fn guide_review_lines(&self) -> Vec<DetailLine> {
        let theme = &self.theme;
        let Some(active) = self.guide.state.active.as_ref() else {
            return Vec::new();
        };
        let Some(entry) = active.current() else {
            return Vec::new();
        };
        let key = |action: &str, fallback: &str| {
            self.action_keys
                .get(action)
                .cloned()
                .filter(|key| !key.is_empty())
                .unwrap_or_else(|| fallback.to_owned())
        };
        let prose = |line: Line| DetailLine {
            line,
            wrap: true,
            action: None,
            markdown: None,
        };
        let hint = || {
            Span::styled(
                format!(
                    " · {} promote · {} discard",
                    key("guide_promote", "p"),
                    key("guide_discard", "x")
                ),
                theme.muted,
            )
        };
        let mut lines = Vec::new();
        if let SectionState::Provisional { origin, .. } = &entry.state {
            let what = match origin {
                bcode_code_review_models::ProvisionalOrigin::Expand => "provisional expansion",
                bcode_code_review_models::ProvisionalOrigin::Ask => "provisional answer",
            };
            lines.push(prose(Line::from_spans(vec![
                Span::styled(theme.icons.guide_unverified, theme.guide.unverified),
                Span::styled(format!(" {what}"), theme.guide.unverified),
                hint(),
            ])));
        }
        let mut turns: BTreeMap<&str, usize> = BTreeMap::new();
        for thread in active.open_threads(&entry.id) {
            lines.push(prose(Line::default()));
            lines.push(prose(Line::from_spans(vec![
                Span::styled("? ", theme.accent),
                Span::styled(thread.question.clone(), theme.tiers.strong),
            ])));
            let Some(session) = thread.session_id.as_deref() else {
                lines.push(prose(Line::from_spans(vec![Span::styled(
                    "asking…",
                    theme.muted,
                )])));
                continue;
            };
            let turn = turns.entry(session).or_default();
            let index = *turn;
            *turn += 1;
            let state = self.agents.state(session);
            let answer = state
                .and_then(|state| state.feed.turns.get(index))
                .map(|turn| turn.answer().to_owned())
                .filter(|answer| !answer.is_empty());
            if let Some(answer) = answer {
                lines.push(DetailLine {
                    markdown: Some(MarkdownBody {
                        source: answer,
                        badges: Vec::new(),
                    }),
                    ..prose(Line::default())
                });
                lines.push(prose(Line::from_spans(vec![
                    Span::styled("answer", theme.muted),
                    hint(),
                ])));
            } else {
                {
                    let status = state.map_or_else(
                        || "connecting…".to_owned(),
                        |state| {
                            if state.phase == crate::agent::AgentPhase::Failed {
                                state.status.clone()
                            } else {
                                "working…".to_owned()
                            }
                        },
                    );
                    lines.push(prose(Line::from_spans(vec![Span::styled(
                        status,
                        theme.muted,
                    )])));
                }
            }
        }
        lines
    }
}

/// `done` as a loader message.
fn guide_done(done: GuideReviewDone) -> Message {
    Message::GuideReview(Box::new(done))
}

/// The finding `id` in `outline`.
fn find_finding<'a>(
    outline: &'a mut [bcode_code_review_models::Section],
    id: &str,
) -> Option<&'a mut bcode_code_review_models::Finding> {
    for section in outline {
        if let Some(found) = section.findings.iter_mut().find(|finding| finding.id == id) {
            return Some(found);
        }
        if let Some(found) = find_finding(&mut section.children, id) {
            return Some(found);
        }
    }
    None
}

/// The files `reviewed` sections of `version` mark viewed, reading hunks at `sources`.
fn files_to_view(
    data: &bcode_code_review::ReviewData,
    repo: std::path::PathBuf,
    sources: &[ResolvedSource],
    version: &GuideVersion,
    reviewed: &BTreeSet<String>,
    mode: MarkFilesViewed,
) -> BTreeSet<ReviewFile> {
    use bcode_code_review::guide::cite::CitationReader;
    let reader = bcode_code_review::guide::cite::ReviewDataReader::new(
        data,
        repo,
        sources,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    coverage::files_to_view(version, reviewed, mode, |(source, path)| {
        let files = reader.diff_files(source).ok()?;
        let file = files.iter().find(|file| file.path == *path)?;
        reader.hunks(source, file).ok()
    })
}

/// Read version `number` of `guide_id`, apply `change`, and save it at the revision read,
/// reading again after a write landed meanwhile.
///
/// # Errors
///
/// * The version is missing, `change` refused, a call failed, or the version kept changing.
async fn edit_version(
    services: &ReviewServices,
    scope: &NoteScope,
    guide_id: &str,
    number: u32,
    change: impl Fn(&mut GuideVersion) -> Result<(), String>,
) -> Result<(), String> {
    let mut last = "the version kept changing".to_owned();
    for _ in 0..WRITE_ATTEMPTS {
        let record = services
            .call(GetGuideVersionRequest {
                repo_path: scope.repo_path.clone(),
                guide_id: guide_id.to_owned(),
                version: Some(number),
            })
            .await
            .map_err(|error| error.message)?
            .version
            .ok_or_else(|| format!("the guide has no v{number}"))?;
        let mut version = record.version;
        change(&mut version)?;
        version.updated_at_ms = crate::assistant::panel::now_ms();
        match services
            .call(SaveGuideVersionRequest {
                repo_path: scope.repo_path.clone(),
                version,
                expected_revision: record.revision,
            })
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) => last = error.message,
        }
    }
    Err(last)
}

/// Add the next version of `guide_id` as a copy of `base` to restructure (with `directions`),
/// making it current; `base` is kept. Returns the new version's number.
///
/// # Errors
///
/// * The base is missing or a call failed.
async fn fork_version(
    services: &ReviewServices,
    scope: &NoteScope,
    guide_id: &str,
    base: u32,
    directions: &str,
) -> Result<u32, String> {
    let record = services
        .call(GetGuideVersionRequest {
            repo_path: scope.repo_path.clone(),
            guide_id: guide_id.to_owned(),
            version: Some(base),
        })
        .await
        .map_err(|error| error.message)?
        .version
        .ok_or_else(|| format!("the guide has no v{base}"))?;
    let now = crate::assistant::panel::now_ms();
    let mut version = record.version;
    version.based_on = Some(base);
    version.status = GuideStatus::Generating;
    version.generation = None;
    version.created_at_ms = now;
    version.updated_at_ms = now;
    if !directions.is_empty() {
        version.origin.directions = Some(directions.to_owned());
    }
    let forked = services
        .call(ForkGuideVersionRequest {
            repo_path: scope.repo_path.clone(),
            version,
            make_current: true,
        })
        .await
        .map_err(|error| error.message)?;
    Ok(forked.version.version.version)
}

/// Record `thread`, ask its question (`prompt`) in its session or a new one titled `title`
/// bound with `binding`, and record the session on the thread.
///
/// # Errors
///
/// * A call that failed.
async fn ask_in_session(
    services: &ReviewServices,
    scope: &NoteScope,
    mut thread: GuideThread,
    binding: bcode_code_review_models::ReviewAssistantBinding,
    (title, prompt, workspace): (String, String, String),
) -> Result<(), String> {
    let save = |thread: GuideThread| async move {
        services
            .call(SaveGuideThreadRequest {
                repo_path: scope.repo_path.clone(),
                thread,
            })
            .await
            .map_err(|error| error.message)
    };
    save(thread.clone()).await?;
    if let Some(session) = thread.session_id.clone() {
        return services
            .host()
            .send_message(session, prompt)
            .await
            .map_err(|error| error.message);
    }
    let session = services
        .host()
        .create_session(
            title,
            scope.repo_path.clone(),
            crate::host::review_session_origin(
                crate::host::ReviewSessionPurpose::Question,
                Some(&workspace),
                None,
            ),
        )
        .await
        .map_err(|error| error.message)?;
    drop(
        crate::assistant::models::apply_default(services, scope.repo_path.clone(), &session).await,
    );
    services
        .call(bcode_code_review_models::BindReviewAssistantRequest {
            repo_path: scope.repo_path.clone(),
            binding: bcode_code_review_models::ReviewAssistantBinding {
                session_id: session.clone(),
                ..binding
            },
        })
        .await
        .map_err(|error| error.message)?;
    thread.session_id = Some(session.clone());
    thread.updated_at_ms = crate::assistant::panel::now_ms();
    save(thread).await?;
    services
        .host()
        .send_message(session, prompt)
        .await
        .map_err(|error| error.message)
}
