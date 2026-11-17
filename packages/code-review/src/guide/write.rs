//! Guide write operations: what `code_review.guide.write` applies to a version, checked against
//! what the writing session may change.
//!
//! A batch is applied to a copy of the version in order; when any operation is refused nothing
//! is written, and every refusal says which operation it was and what the session may do
//! instead. Inline citations are checked before the batch is applied (see [`super::cite`]) and
//! arrive here with their results, in the order [`inline_citations`] lists them.
//!
//! Section ids are stable: a removed section's id is retired in its version and never used
//! again. Body tokens `[[c7]]` name citations by id; `[[2]]` names the second citation of the
//! same operation's `citations` list and is rewritten to its id.

use super::template::{is_adaptive, kind_of_key};
use bcode_code_review_models::{
    Citation, CitationCheck, CitationRequest, ClaimCheck, ClaimStatus, Finding, FindingKind,
    GrantMode, GuideAreaStatus, GuideStatus, GuideVersion, ProvisionalOrigin, ReviewThreadSeverity,
    Section, SectionKind, SectionState, all_sections, find_section,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Operations one write takes at most.
pub const MAX_OPS: usize = 50;
/// Bytes of a section or citation id at most.
const MAX_ID_BYTES: usize = 64;

/// Refusal code: the operation is outside what the session may change.
pub const OUTSIDE_GRANT: &str = "outside_grant";
/// Refusal code: the operation cannot be applied as given.
pub const INVALID_OP: &str = "invalid_op";

/// One write operation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuideOp {
    /// Set the outline's structure: planned sections for new ids, existing sections kept with
    /// their content.
    SetOutline {
        /// The whole tree.
        sections: Vec<OutlineInput>,
    },
    /// Write a section: create it (kind and title required) or change the fields given.
    PutSection(SectionWrite),
    /// Add a provisional expansion under a section.
    AddExpansion(ExpansionWrite),
    /// Make a provisional section written.
    Promote {
        /// The section.
        section: String,
    },
    /// Remove a provisional section.
    Discard {
        /// The section.
        section: String,
    },
    /// Remove a section and its children.
    RemoveSection {
        /// The section.
        id: String,
    },
    /// Move a section.
    MoveSection {
        /// The section.
        id: String,
        /// The new parent; top level when absent.
        #[serde(default)]
        parent: Option<String>,
        /// The sibling it follows; first when absent.
        #[serde(default)]
        after: Option<String>,
    },
    /// Record a verifier's claim checks on a section (replacing earlier ones).
    RecordChecks {
        /// The section.
        section: String,
        /// The checks.
        checks: Vec<CheckInput>,
    },
    /// Write one fan-out area's result.
    AreaResult {
        /// The area.
        area_id: String,
        /// Its sections.
        #[serde(default)]
        sections: Vec<SectionWrite>,
        /// Where it stands; complete by default.
        #[serde(default)]
        status: Option<AreaStatusInput>,
        /// Why it failed.
        #[serde(default)]
        message: Option<String>,
    },
    /// Set the version's status.
    SetStatus {
        /// The status.
        status: StatusInput,
        /// Why, for a failure.
        #[serde(default)]
        message: Option<String>,
    },
}

impl GuideOp {
    /// The operation's name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SetOutline { .. } => "set_outline",
            Self::PutSection(_) => "put_section",
            Self::AddExpansion(_) => "add_expansion",
            Self::Promote { .. } => "promote",
            Self::Discard { .. } => "discard",
            Self::RemoveSection { .. } => "remove_section",
            Self::MoveSection { .. } => "move_section",
            Self::RecordChecks { .. } => "record_checks",
            Self::AreaResult { .. } => "area_result",
            Self::SetStatus { .. } => "set_status",
        }
    }
}

/// A section of an outline.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OutlineInput {
    /// Section id.
    pub id: String,
    /// Kind: `purpose`, `scope`, `reading_order`, `step`, `findings`, `note`, `topic` (with
    /// `key`), or an adaptive or template section key (`risks`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Topic key, for `topic`.
    #[serde(default)]
    pub key: Option<String>,
    /// Title.
    #[serde(default)]
    pub title: Option<String>,
    /// Child sections.
    #[serde(default)]
    pub children: Vec<Self>,
}

/// A section to write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SectionWrite {
    /// Section id.
    pub id: String,
    /// The parent, for a new section or a move; top level when absent.
    #[serde(default)]
    pub parent: Option<String>,
    /// The sibling it follows, for a new section or a move; last when absent.
    #[serde(default)]
    pub after: Option<String>,
    /// Kind (see [`OutlineInput::kind`]).
    #[serde(default)]
    pub kind: Option<String>,
    /// Topic key, for `topic`.
    #[serde(default)]
    pub key: Option<String>,
    /// Title.
    #[serde(default)]
    pub title: Option<String>,
    /// Markdown body with citation tokens.
    #[serde(default)]
    pub body: Option<String>,
    /// Citations in reading order: ids or inline citations.
    #[serde(default)]
    pub citations: Option<Vec<CiteInput>>,
    /// Findings (replacing earlier ones).
    #[serde(default)]
    pub findings: Option<Vec<FindingInput>>,
}

/// An expansion to add.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ExpansionWrite {
    /// The section it expands.
    pub under: String,
    /// Its id; `<under>.x<n>` by default.
    #[serde(default)]
    pub id: Option<String>,
    /// Title.
    pub title: String,
    /// Markdown body with citation tokens.
    #[serde(default)]
    pub body: String,
    /// Citations: ids or inline citations.
    #[serde(default)]
    pub citations: Vec<CiteInput>,
    /// Findings.
    #[serde(default)]
    pub findings: Vec<FindingInput>,
}

/// A citation in a write: an existing id or an inline citation to check.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum CiteInput {
    /// An existing citation id.
    Id(String),
    /// A citation to check and store.
    Inline(InlineCitation),
}

/// An inline citation, optionally with the id the writer wants for it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InlineCitation {
    /// The id wanted; the next free `c<n>` when absent.
    #[serde(default)]
    pub id: Option<String>,
    /// What is cited.
    #[serde(flatten)]
    pub request: CitationRequest,
}

/// A finding's citation: an id, the position (from 1) of a citation in the same operation's
/// list, or an inline citation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FindingCite {
    /// Position in the operation's `citations`.
    Index(usize),
    /// An existing citation id.
    Id(String),
    /// A citation to check and store.
    Inline(InlineCitation),
}

/// A finding to write.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FindingInput {
    /// Its id; the next free `f<n>` when absent.
    #[serde(default)]
    pub id: Option<String>,
    /// What it is.
    pub kind: FindingKind,
    /// How severe.
    #[serde(default)]
    pub severity: Option<ReviewThreadSeverity>,
    /// Text.
    pub text: String,
    /// Citations.
    #[serde(default)]
    pub citations: Vec<FindingCite>,
}

/// A claim check to record.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckInput {
    /// The claim.
    pub claim: String,
    /// The judgement.
    pub status: ClaimStatus,
    /// Why.
    #[serde(default)]
    pub note: String,
    /// Citation ids checked.
    #[serde(default)]
    pub citations: Vec<String>,
}

/// A status a writer sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusInput {
    /// Still writing.
    Generating,
    /// Done.
    Complete,
    /// Could not finish.
    Failed,
    /// Stopped.
    Cancelled,
}

/// An area status a writer sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaStatusInput {
    /// Still writing.
    Running,
    /// Done.
    Complete,
    /// Could not finish.
    Failed,
}

impl AreaStatusInput {
    /// The stored status.
    #[must_use]
    pub const fn status(self) -> GuideAreaStatus {
        match self {
            Self::Running => GuideAreaStatus::Running,
            Self::Complete => GuideAreaStatus::Complete,
            Self::Failed => GuideAreaStatus::Failed,
        }
    }
}

/// Why an operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpError {
    /// The operation's position in the batch, from 0.
    pub op_index: usize,
    /// [`OUTSIDE_GRANT`] or [`INVALID_OP`].
    pub code: &'static str,
    /// One actionable sentence.
    pub message: String,
}

/// What a stored inline citation became.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CitationOutcome {
    /// Its id.
    pub id: String,
    /// The operation that wrote it.
    pub op_index: usize,
    /// The check's result.
    #[serde(flatten)]
    pub check: CitationCheck,
}

/// What a batch did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApplyOutcome {
    /// Sections created or changed, in order.
    pub sections_written: Vec<String>,
    /// Inline citations stored.
    pub citations: Vec<CitationOutcome>,
}

/// Who is writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Writer<'a> {
    /// What the session may change.
    pub mode: &'a GrantMode,
    /// The session, recorded on claim checks.
    pub session_id: Option<&'a str>,
    /// Now, in milliseconds since the Unix epoch.
    pub now_ms: u64,
}

/// Every inline citation of `op` in the order [`apply`] consumes their checks: the section's
/// (or expansion's) list, then each finding's.
#[must_use]
pub fn inline_citations(op: &GuideOp) -> Vec<&CitationRequest> {
    fn of_section<'a>(
        citations: Option<&'a [CiteInput]>,
        findings: Option<&'a [FindingInput]>,
        out: &mut Vec<&'a CitationRequest>,
    ) {
        for cite in citations.unwrap_or_default() {
            if let CiteInput::Inline(inline) = cite {
                out.push(&inline.request);
            }
        }
        for finding in findings.unwrap_or_default() {
            for cite in &finding.citations {
                if let FindingCite::Inline(inline) = cite {
                    out.push(&inline.request);
                }
            }
        }
    }
    let mut out = Vec::new();
    match op {
        GuideOp::PutSection(write) => of_section(
            write.citations.as_deref(),
            write.findings.as_deref(),
            &mut out,
        ),
        GuideOp::AddExpansion(write) => {
            of_section(Some(&write.citations), Some(&write.findings), &mut out);
        }
        GuideOp::AreaResult { sections, .. } => {
            for write in sections {
                of_section(
                    write.citations.as_deref(),
                    write.findings.as_deref(),
                    &mut out,
                );
            }
        }
        _ => {}
    }
    out
}

/// Every inline citation of `ops`, in order.
#[must_use]
pub fn batch_citations(ops: &[GuideOp]) -> Vec<CitationRequest> {
    ops.iter()
        .flat_map(|op| inline_citations(op).into_iter().cloned())
        .collect()
}

/// The outline, citations and retired ids a write changes: a version's or an area's.
pub struct Doc<'a> {
    /// The section tree.
    pub outline: &'a mut Vec<Section>,
    /// Citations, by id.
    pub citations: &'a mut BTreeMap<String, Citation>,
    /// Retired section ids.
    pub retired: &'a mut Vec<String>,
    /// Whether an inline citation naming a stored id at the same lines replaces the stored
    /// citation (writers of a whole version or area). A narrower writer leaves it as it is, for
    /// other sections cite it too, and stores a different one under a free id.
    pub replace_citations: bool,
}

/// Apply `ops` to `version` as `writer`; `checks` are the results of checking
/// [`batch_citations`] of `ops`, in order. On any refusal the version is left as it was and
/// every refusal is returned.
///
/// # Errors
///
/// * Every [`OpError`] of the batch.
pub fn apply(
    version: &mut GuideVersion,
    ops: &[GuideOp],
    checks: &[CitationCheck],
    writer: &Writer<'_>,
) -> Result<ApplyOutcome, Vec<OpError>> {
    let mut work = version.clone();
    let mut outcome = ApplyOutcome::default();
    let mut errors = Vec::new();
    let mut offset = 0;
    for (index, op) in ops.iter().enumerate() {
        let count = inline_citations(op).len();
        let op_checks = checks.get(offset..offset + count).unwrap_or_default();
        offset += count;
        let mut staged = work.clone();
        let mut staged_outcome = outcome.clone();
        let result = allowed(writer.mode, op, &staged).and_then(|()| {
            apply_one(
                &mut staged,
                op,
                index,
                op_checks,
                writer,
                &mut staged_outcome,
            )
        });
        match result {
            Ok(()) => {
                work = staged;
                outcome = staged_outcome;
            }
            Err((code, message)) => errors.push(OpError {
                op_index: index,
                code,
                message,
            }),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    prune_citations(&work.outline, &mut work.citations);
    work.updated_at_ms = writer.now_ms;
    *version = work;
    Ok(outcome)
}

type OpResult = Result<(), (&'static str, String)>;

fn invalid(message: impl Into<String>) -> (&'static str, String) {
    (INVALID_OP, message.into())
}

fn outside(message: impl Into<String>) -> (&'static str, String) {
    (OUTSIDE_GRANT, message.into())
}

/// Whether `section` is `ancestor` or under it.
fn within(outline: &[Section], ancestor: &str, section: &str) -> bool {
    find_section(outline, ancestor).is_some_and(|ancestor| {
        ancestor.id == section || find_section(&ancestor.children, section).is_some()
    })
}

fn is_written(section: &Section) -> bool {
    section.state == SectionState::Written
}

/// Whether `mode` lets the session apply `op` to `version`.
fn allowed(mode: &GrantMode, op: &GuideOp, version: &GuideVersion) -> OpResult {
    let structure_of_written = |id: &str| {
        version
            .section(id)
            .is_some_and(|section| !matches!(section.state, SectionState::Planned))
    };
    match mode {
        GrantMode::Restructure => Ok(()),
        GrantMode::Generate => match op {
            GuideOp::RemoveSection { id } | GuideOp::MoveSection { id, .. }
                if structure_of_written(id) =>
            {
                Err(outside(format!(
                    "{id} is written; moving or removing written sections needs the user to restructure the guide (a new version)"
                )))
            }
            GuideOp::PutSection(write)
                if (write.parent.is_some() || write.after.is_some())
                    && version.section(&write.id).is_some_and(is_written)
                    && !same_place(
                        version,
                        &write.id,
                        write.parent.as_deref(),
                        write.after.as_deref(),
                    ) =>
            {
                Err(outside(format!(
                    "{} is written; moving it needs the user to restructure the guide",
                    write.id
                )))
            }
            _ => Ok(()),
        },
        GrantMode::Expand { section_id } | GrantMode::Ask { section_id } => {
            let ask = matches!(mode, GrantMode::Ask { .. });
            let here = |id: &str| within(&version.outline, section_id, id);
            let fine = match op {
                GuideOp::AddExpansion(write) => write.under == *section_id,
                GuideOp::Promote { section } => ask && here(section) && section != section_id,
                GuideOp::Discard { section } => here(section) && section != section_id,
                _ => false,
            };
            if fine {
                Ok(())
            } else {
                Err(outside(format!(
                    "this conversation may only add under {section_id} (add_expansion{}); ask the user to restructure the guide for anything else",
                    if ask { ", promote" } else { "" }
                )))
            }
        }
        GrantMode::Refresh { section_ids } => match op {
            GuideOp::PutSection(write)
                if section_ids.contains(&write.id)
                    && version.section(&write.id).is_some()
                    && (write.parent.is_none() && write.after.is_none()) =>
            {
                Ok(())
            }
            _ => Err(outside(format!(
                "this conversation may only rewrite {} in place (put_section); ask the user to restructure the guide for anything else",
                section_ids.join(", ")
            ))),
        },
        GrantMode::Verify => match op {
            GuideOp::RecordChecks { .. } => Ok(()),
            _ => Err(outside(
                "this conversation verifies the guide and may only record claim checks (record_checks)",
            )),
        },
        GrantMode::Area { area_id } => match op {
            GuideOp::AreaResult {
                area_id: wanted, ..
            } if wanted == area_id => Ok(()),
            _ => Err(outside(format!(
                "this conversation writes the area {area_id} and may only write its result (area_result with area_id {area_id})"
            ))),
        },
    }
}

/// Whether placing `id` under `parent` after `after` leaves it where it is.
fn same_place(version: &GuideVersion, id: &str, parent: Option<&str>, after: Option<&str>) -> bool {
    let Some((current_parent, siblings)) = locate(&version.outline, id, None) else {
        return false;
    };
    if current_parent.as_deref() != parent {
        return false;
    }
    let position = siblings.iter().position(|sibling| sibling == id);
    let previous =
        position.and_then(|position| position.checked_sub(1).map(|p| siblings[p].clone()));
    after.is_none() || previous.as_deref() == after
}

/// The parent id of `id` and its siblings' ids (itself included), when it is in `sections`.
fn locate(
    sections: &[Section],
    id: &str,
    parent: Option<&str>,
) -> Option<(Option<String>, Vec<String>)> {
    if sections.iter().any(|section| section.id == id) {
        return Some((
            parent.map(str::to_owned),
            sections.iter().map(|section| section.id.clone()).collect(),
        ));
    }
    sections
        .iter()
        .find_map(|section| locate(&section.children, id, Some(&section.id)))
}

fn apply_one(
    version: &mut GuideVersion,
    op: &GuideOp,
    index: usize,
    checks: &[CitationCheck],
    writer: &Writer<'_>,
    outcome: &mut ApplyOutcome,
) -> OpResult {
    let restructure = matches!(writer.mode, GrantMode::Restructure);
    let mut doc = Doc {
        outline: &mut version.outline,
        citations: &mut version.citations,
        retired: &mut version.retired_sections,
        replace_citations: matches!(writer.mode, GrantMode::Generate | GrantMode::Restructure),
    };
    match op {
        GuideOp::SetOutline { sections } => set_outline(&mut doc, sections, restructure)?,
        GuideOp::PutSection(write) => {
            let mut checks = checks.iter();
            put_section(&mut doc, write, index, &mut checks, outcome, None)?;
        }
        GuideOp::AddExpansion(write) => {
            let origin = match writer.mode {
                GrantMode::Ask { .. } => ProvisionalOrigin::Ask,
                _ => ProvisionalOrigin::Expand,
            };
            add_expansion(&mut doc, write, index, checks, outcome, origin)?;
        }
        GuideOp::Promote { section } => {
            let found = find_mut(doc.outline, section)
                .ok_or_else(|| invalid(format!("no section {section}")))?;
            if !matches!(found.state, SectionState::Provisional { .. }) {
                return Err(invalid(format!("{section} is not provisional")));
            }
            found.state = SectionState::Written;
            outcome.sections_written.push(section.clone());
        }
        GuideOp::Discard { section } => {
            let found = find_section(doc.outline, section)
                .ok_or_else(|| invalid(format!("no section {section}")))?;
            if !matches!(found.state, SectionState::Provisional { .. }) {
                return Err(invalid(format!(
                    "{section} is not provisional; only expansions and answers are discarded"
                )));
            }
            remove(&mut doc, section);
        }
        GuideOp::RemoveSection { id } => {
            if find_section(doc.outline, id).is_none() {
                return Err(invalid(format!("no section {id}")));
            }
            remove(&mut doc, id);
        }
        GuideOp::MoveSection { id, parent, after } => {
            move_section(doc.outline, id, parent.as_deref(), after.as_deref())?;
        }
        GuideOp::RecordChecks { section, checks } => {
            let known: BTreeSet<String> = doc.citations.keys().cloned().collect();
            let found = find_mut(doc.outline, section)
                .ok_or_else(|| invalid(format!("no section {section}")))?;
            let mut recorded = Vec::with_capacity(checks.len());
            for check in checks {
                if let Some(missing) = check.citations.iter().find(|id| !known.contains(*id)) {
                    return Err(invalid(format!(
                        "the check of {:?} names {missing}, which the guide does not have",
                        check.claim
                    )));
                }
                recorded.push(ClaimCheck {
                    claim: check.claim.clone(),
                    status: check.status,
                    note: check.note.clone(),
                    citations: check.citations.clone(),
                    by_session: writer.session_id.map(str::to_owned),
                });
            }
            found.checks = recorded;
            outcome.sections_written.push(section.clone());
        }
        GuideOp::AreaResult { .. } => {
            return Err(invalid(
                "area_result writes an area, not the version; send it alone in its own write",
            ));
        }
        GuideOp::SetStatus { status, message } => {
            version.status = match status {
                StatusInput::Generating => GuideStatus::Generating,
                StatusInput::Complete => GuideStatus::Complete,
                StatusInput::Cancelled => GuideStatus::Cancelled,
                StatusInput::Failed => GuideStatus::Failed {
                    message: message
                        .clone()
                        .filter(|message| !message.trim().is_empty())
                        .unwrap_or_else(|| "the writer could not finish".to_owned()),
                },
            };
        }
    }
    Ok(())
}

/// Apply one area result's sections to an area document.
///
/// # Errors
///
/// * The refusal's code and message.
pub fn apply_area_sections(
    doc: &mut Doc<'_>,
    sections: &[SectionWrite],
    checks: &[CitationCheck],
    outcome: &mut ApplyOutcome,
) -> Result<(), (&'static str, String)> {
    let mut checks = checks.iter();
    for write in sections {
        put_section(doc, write, 0, &mut checks, outcome, None)?;
    }
    prune_citations(doc.outline, doc.citations);
    Ok(())
}

fn find_mut<'a>(sections: &'a mut [Section], id: &str) -> Option<&'a mut Section> {
    for section in sections {
        if section.id == id {
            return Some(section);
        }
        if let Some(found) = find_mut(&mut section.children, id) {
            return Some(found);
        }
    }
    None
}

/// Take `id` (with its children) out of the tree.
fn detach(sections: &mut Vec<Section>, id: &str) -> Option<Section> {
    if let Some(position) = sections.iter().position(|section| section.id == id) {
        return Some(sections.remove(position));
    }
    sections
        .iter_mut()
        .find_map(|section| detach(&mut section.children, id))
}

/// Insert `section` under `parent` (top level when `None`) after `after` (last when `None`,
/// unless `first`).
fn insert(
    outline: &mut Vec<Section>,
    section: Section,
    parent: Option<&str>,
    after: Option<&str>,
    first: bool,
) -> OpResult {
    let siblings = match parent {
        None => outline,
        Some(parent) => {
            &mut find_mut(outline, parent)
                .ok_or_else(|| invalid(format!("no parent section {parent}")))?
                .children
        }
    };
    let position = match after {
        Some(after) => {
            siblings
                .iter()
                .position(|sibling| sibling.id == after)
                .ok_or_else(|| {
                    invalid(format!(
                        "{after} is not a child of {}",
                        parent.unwrap_or("the top level")
                    ))
                })?
                + 1
        }
        None if first => 0,
        None => siblings.len(),
    };
    siblings.insert(position, section);
    Ok(())
}

fn remove(doc: &mut Doc<'_>, id: &str) {
    if let Some(section) = detach(doc.outline, id) {
        for gone in all_sections(std::slice::from_ref(&section)) {
            if !doc.retired.contains(&gone.id) {
                doc.retired.push(gone.id.clone());
            }
        }
    }
}

fn move_section(
    outline: &mut Vec<Section>,
    id: &str,
    parent: Option<&str>,
    after: Option<&str>,
) -> OpResult {
    if parent.is_some_and(|parent| within(outline, id, parent)) {
        return Err(invalid(format!("{id} cannot move under itself")));
    }
    if after == Some(id) {
        return Err(invalid(format!("{id} cannot follow itself")));
    }
    let saved = outline.clone();
    let section = detach(outline, id).ok_or_else(|| invalid(format!("no section {id}")))?;
    if let Err(error) = insert(outline, section, parent, after, true) {
        *outline = saved;
        return Err(error);
    }
    Ok(())
}

fn check_id(id: &str, what: &str) -> OpResult {
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(invalid(format!(
            "{what} id {id:?} must be 1 to {MAX_ID_BYTES} letters, digits, '.', '_' or '-'"
        )));
    }
    Ok(())
}

fn fresh_section_id(doc: &Doc<'_>, id: &str) -> OpResult {
    check_id(id, "section")?;
    if doc.retired.iter().any(|retired| retired == id) {
        return Err(invalid(format!(
            "section id {id} was removed from this version and is not used again; choose another id"
        )));
    }
    Ok(())
}

/// The kind `kind` (and `key`) names.
fn parse_kind(
    kind: Option<&str>,
    key: Option<&str>,
) -> Result<Option<SectionKind>, (&'static str, String)> {
    let Some(kind) = kind.map(str::trim).filter(|kind| !kind.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(match kind {
        "purpose" | "scope" | "reading_order" => kind_of_key(kind),
        "step" => SectionKind::Step,
        "findings" => SectionKind::Findings,
        "note" => SectionKind::Note,
        "expansion" => SectionKind::Expansion,
        "topic" => {
            let key = key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .ok_or_else(|| invalid("a topic section needs a `key` (risks, security, ...)"))?;
            SectionKind::Topic {
                key: key.to_owned(),
            }
        }
        other
            if is_adaptive(other) || other.chars().all(|c| c.is_ascii_lowercase() || c == '_') =>
        {
            SectionKind::Topic {
                key: other.to_owned(),
            }
        }
        other => {
            return Err(invalid(format!(
                "unknown section kind {other:?}; use purpose, scope, reading_order, step, findings, note, or a topic key"
            )));
        }
    }))
}

/// Every section `inputs` names, with its parent.
fn named(inputs: &[OutlineInput], parent: Option<&str>, out: &mut Vec<(String, Option<String>)>) {
    for input in inputs {
        out.push((input.id.clone(), parent.map(str::to_owned)));
        named(&input.children, Some(&input.id), out);
    }
}
/// The sections `inputs` describe: existing ones keep their content, new ones are planned.
fn build(
    inputs: &[OutlineInput],
    existing: &BTreeMap<String, Section>,
    retired: &[String],
) -> Result<Vec<Section>, (&'static str, String)> {
    inputs
        .iter()
        .map(|input| {
            let kind = parse_kind(input.kind.as_deref(), input.key.as_deref())?;
            let children = build(&input.children, existing, retired)?;
            if let Some(section) = existing.get(&input.id) {
                let mut section = section.clone();
                if section.state == SectionState::Planned {
                    if let Some(kind) = kind {
                        section.kind = kind;
                    }
                    if let Some(title) = &input.title {
                        section.title.clone_from(title);
                    }
                }
                section.children = children;
                return Ok(section);
            }
            check_id(&input.id, "section")?;
            if retired.contains(&input.id) {
                return Err(invalid(format!(
                    "section id {} was removed from this version and is not used again",
                    input.id
                )));
            }
            Ok(Section {
                id: input.id.clone(),
                kind: kind
                    .ok_or_else(|| invalid(format!("the new section {} needs a kind", input.id)))?,
                title: input
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .ok_or_else(|| {
                        invalid(format!("the new section {} needs a title", input.id))
                    })?,
                body: String::new(),
                findings: Vec::new(),
                citations: Vec::new(),
                children,
                state: SectionState::Planned,
                refreshed_from: None,
                checks: Vec::new(),
            })
        })
        .collect()
}
fn set_outline(doc: &mut Doc<'_>, sections: &[OutlineInput], restructure: bool) -> OpResult {
    let mut listed = Vec::new();
    named(sections, None, &mut listed);
    let mut seen = BTreeSet::new();
    for (id, _) in &listed {
        if !seen.insert(id.as_str()) {
            return Err(invalid(format!("the outline names {id} twice")));
        }
    }
    let existing: BTreeMap<String, Section> = all_sections(doc.outline)
        .into_iter()
        .map(|section| {
            let mut bare = section.clone();
            bare.children = Vec::new();
            (section.id.clone(), bare)
        })
        .collect();
    let parents: BTreeMap<String, Option<String>> = existing
        .keys()
        .filter_map(|id| locate(doc.outline, id, None).map(|(parent, _)| (id.clone(), parent)))
        .collect();
    for (id, section) in &existing {
        if section.state == SectionState::Planned {
            continue;
        }
        match listed.iter().find(|(listed, _)| listed == id) {
            None if !restructure => {
                return Err(outside(format!(
                    "the outline leaves out the written section {id}; list it, or ask the user to restructure the guide"
                )));
            }
            Some((_, parent)) if !restructure && parents.get(id) != Some(parent) => {
                return Err(outside(format!(
                    "the outline moves the written section {id}; keep it under {}, or ask the user to restructure the guide",
                    parents
                        .get(id)
                        .and_then(Option::as_deref)
                        .unwrap_or("the top level")
                )));
            }
            _ => {}
        }
    }
    let outline = build(sections, &existing, doc.retired)?;
    let kept: BTreeSet<String> = listed.into_iter().map(|(id, _)| id).collect();
    for id in existing.keys() {
        if !kept.contains(id) && !doc.retired.contains(id) {
            doc.retired.push(id.clone());
        }
    }
    *doc.outline = outline;
    Ok(())
}

/// Whether `a` and `b` say the same about the same lines: the same text, revision and outcome.
fn same_evidence(a: &Citation, b: &Citation) -> bool {
    a.quote_hash == b.quote_hash
        && a.excerpt == b.excerpt
        && a.revision == b.revision
        && a.relation == b.relation
        && std::mem::discriminant(&a.verification) == std::mem::discriminant(&b.verification)
}

/// Store one checked inline citation, under the id wanted when it is free (or names the same
/// lines, and the writer may replace it or it says the same), else the next free `c<n>`.
fn store_citation(
    doc: &mut Doc<'_>,
    wanted: Option<&str>,
    check: &CitationCheck,
    index: usize,
    outcome: &mut ApplyOutcome,
) -> Result<String, (&'static str, String)> {
    let mut citation = check.citation.clone();
    let wanted = match wanted.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => {
            check_id(id, "citation")?;
            match doc.citations.get(id) {
                Some(existing) => {
                    let same = existing.path == citation.path
                        && existing.side == citation.side
                        && existing.lines == citation.lines
                        && existing.source_id == citation.source_id;
                    if !same {
                        return Err(invalid(format!(
                            "citation id {id} is already {}:{}-{}; cite it as \"{id}\" or leave out the id",
                            existing.path, existing.lines.start, existing.lines.end
                        )));
                    }
                    (doc.replace_citations || same_evidence(existing, &citation))
                        .then(|| id.to_owned())
                }
                None => Some(id.to_owned()),
            }
        }
        None => None,
    };
    let id = if let Some(id) = wanted {
        id
    } else {
        let mut next = doc.citations.len() + 1;
        loop {
            let candidate = format!("c{next}");
            if !doc.citations.contains_key(&candidate) {
                break candidate;
            }
            next += 1;
        }
    };
    citation.id.clone_from(&id);
    doc.citations.insert(id.clone(), citation);
    outcome.citations.push(CitationOutcome {
        id: id.clone(),
        op_index: index,
        check: check.clone(),
    });
    Ok(id)
}

/// Resolve a section's citation list to ids, storing inline ones.
fn resolve_cites<'c>(
    doc: &mut Doc<'_>,
    cites: &[CiteInput],
    index: usize,
    checks: &mut impl Iterator<Item = &'c CitationCheck>,
    outcome: &mut ApplyOutcome,
) -> Result<Vec<String>, (&'static str, String)> {
    let mut ids = Vec::with_capacity(cites.len());
    for cite in cites {
        let id = match cite {
            CiteInput::Id(id) => {
                if !doc.citations.contains_key(id) {
                    return Err(invalid(format!(
                        "no citation {id}; cite with an inline citation object or an existing id"
                    )));
                }
                id.clone()
            }
            CiteInput::Inline(inline) => {
                let check = checks
                    .next()
                    .ok_or_else(|| invalid("a citation was not checked"))?;
                store_citation(doc, inline.id.as_deref(), check, index, outcome)?
            }
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn resolve_findings<'c>(
    doc: &mut Doc<'_>,
    findings: &[FindingInput],
    listed: &[String],
    index: usize,
    checks: &mut impl Iterator<Item = &'c CitationCheck>,
    outcome: &mut ApplyOutcome,
) -> Result<Vec<Finding>, (&'static str, String)> {
    let mut used: BTreeSet<String> = all_sections(doc.outline)
        .into_iter()
        .flat_map(|section| section.findings.iter().map(|finding| finding.id.clone()))
        .collect();
    let mut out = Vec::with_capacity(findings.len());
    for finding in findings {
        let mut ids = Vec::new();
        for cite in &finding.citations {
            let id = match cite {
                FindingCite::Index(position) => listed
                    .get(position.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| {
                        invalid(format!(
                            "a finding cites [{position}], but the operation lists {} citations",
                            listed.len()
                        ))
                    })?,
                FindingCite::Id(id) => {
                    if !doc.citations.contains_key(id) {
                        return Err(invalid(format!(
                            "a finding cites {id}, which does not exist"
                        )));
                    }
                    id.clone()
                }
                FindingCite::Inline(inline) => {
                    let check = checks
                        .next()
                        .ok_or_else(|| invalid("a citation was not checked"))?;
                    store_citation(doc, inline.id.as_deref(), check, index, outcome)?
                }
            };
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let id = if let Some(id) = finding
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            check_id(id, "finding")?;
            id.to_owned()
        } else {
            let mut next = used.len() + 1;
            loop {
                let candidate = format!("f{next}");
                if !used.contains(&candidate) {
                    break candidate;
                }
                next += 1;
            }
        };
        used.insert(id.clone());
        out.push(Finding {
            id,
            kind: finding.kind,
            severity: finding.severity,
            text: finding.text.clone(),
            citations: ids,
            drafted_thread_id: None,
        });
    }
    Ok(out)
}

/// `body` with `[[n]]` tokens rewritten to the ids of `listed` (from 1).
fn rewrite_tokens(body: &str, listed: &[String]) -> Result<String, (&'static str, String)> {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let token = &after[..end];
        out.push_str("[[");
        if let Ok(position) = token.trim().parse::<usize>() {
            let id = listed.get(position.wrapping_sub(1)).ok_or_else(|| {
                invalid(format!(
                    "the body cites [[{position}]], but the operation lists {} citations",
                    listed.len()
                ))
            })?;
            out.push_str(id);
        } else {
            out.push_str(token);
        }
        out.push_str("]]");
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn put_section<'c>(
    doc: &mut Doc<'_>,
    write: &SectionWrite,
    index: usize,
    checks: &mut impl Iterator<Item = &'c CitationCheck>,
    outcome: &mut ApplyOutcome,
    provisional: Option<SectionState>,
) -> OpResult {
    let kind = parse_kind(write.kind.as_deref(), write.key.as_deref())?;
    let exists = find_section(doc.outline, &write.id).is_some();
    if !exists {
        fresh_section_id(doc, &write.id)?;
    }
    let listed = match &write.citations {
        Some(cites) => Some(resolve_cites(doc, cites, index, checks, outcome)?),
        None => None,
    };
    let empty = Vec::new();
    let token_ids = listed.as_ref().unwrap_or(&empty);
    let findings = match &write.findings {
        Some(findings) => Some(resolve_findings(
            doc, findings, token_ids, index, checks, outcome,
        )?),
        None => None,
    };
    let body = match &write.body {
        Some(body) => Some(rewrite_tokens(body, token_ids)?),
        None => None,
    };
    if exists {
        let section = find_mut(doc.outline, &write.id)
            .ok_or_else(|| invalid(format!("no section {}", write.id)))?;
        if let Some(kind) = kind {
            section.kind = kind;
        }
        if let Some(title) = write
            .title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
        {
            section.title.clone_from(title);
        }
        if let Some(body) = body {
            section.body = body;
        }
        if let Some(listed) = listed {
            section.citations = listed;
        }
        if let Some(findings) = findings {
            section.findings = findings;
        }
        if section.state == SectionState::Planned {
            section.state = provisional.unwrap_or(SectionState::Written);
        }
        if write.parent.is_some() || write.after.is_some() {
            move_section(
                doc.outline,
                &write.id,
                write.parent.as_deref(),
                write.after.as_deref(),
            )?;
        }
    } else {
        let section = Section {
            id: write.id.clone(),
            kind: kind.ok_or_else(|| {
                invalid(format!(
                    "{} is a new section and needs a kind (step, purpose, risks, ...)",
                    write.id
                ))
            })?,
            title: write
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .ok_or_else(|| {
                    invalid(format!("{} is a new section and needs a title", write.id))
                })?,
            body: body.unwrap_or_default(),
            findings: findings.unwrap_or_default(),
            citations: listed.unwrap_or_default(),
            children: Vec::new(),
            state: provisional.unwrap_or(SectionState::Written),
            refreshed_from: None,
            checks: Vec::new(),
        };
        insert(
            doc.outline,
            section,
            write.parent.as_deref(),
            write.after.as_deref(),
            false,
        )?;
    }
    outcome.sections_written.push(write.id.clone());
    Ok(())
}

fn add_expansion(
    doc: &mut Doc<'_>,
    write: &ExpansionWrite,
    index: usize,
    checks: &[CitationCheck],
    outcome: &mut ApplyOutcome,
    origin: ProvisionalOrigin,
) -> OpResult {
    let parent = find_section(doc.outline, &write.under)
        .ok_or_else(|| invalid(format!("no section {}", write.under)))?;
    let id = if let Some(id) = write
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if find_section(doc.outline, id).is_some() {
            return Err(invalid(format!("section {id} exists; choose another id")));
        }
        id.to_owned()
    } else {
        let mut next = parent.children.len() + 1;
        loop {
            let candidate = format!("{}.x{next}", write.under);
            if find_section(doc.outline, &candidate).is_none() && !doc.retired.contains(&candidate)
            {
                break candidate;
            }
            next += 1;
        }
    };
    let section = SectionWrite {
        id,
        parent: Some(write.under.clone()),
        after: None,
        kind: Some("expansion".to_owned()),
        key: None,
        title: Some(write.title.clone()),
        body: Some(write.body.clone()),
        citations: Some(write.citations.clone()),
        findings: Some(write.findings.clone()),
    };
    let mut checks = checks.iter();
    put_section(
        doc,
        &section,
        index,
        &mut checks,
        outcome,
        Some(SectionState::Provisional {
            origin,
            thread_id: None,
        }),
    )
}

/// The citation ids a body's tokens name.
#[must_use]
pub fn body_tokens(body: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let token = after[..end].trim();
        if !token.is_empty() && !ids.iter().any(|id: &String| id == token) {
            ids.push(token.to_owned());
        }
        rest = &after[end + 2..];
    }
    ids
}

/// Drop citations nothing uses any more (no section, finding, check, or body token).
fn prune_citations(outline: &[Section], citations: &mut BTreeMap<String, Citation>) {
    let mut used: BTreeSet<String> = BTreeSet::new();
    for section in all_sections(outline) {
        used.extend(section.cited_ids().into_iter().map(str::to_owned));
        used.extend(body_tokens(&section.body));
        for check in &section.checks {
            used.extend(check.citations.iter().cloned());
        }
    }
    citations.retain(|id, _| used.contains(id));
}

#[cfg(test)]
mod tests;
