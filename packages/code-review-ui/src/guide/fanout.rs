//! A guide written by child agents: what the app decides from a version's fan-out progress
//! ([`GenerationState`]) and what the panel and the guide card show of it.
//!
//! The progress lives in the stored version, so a fan-out survives the app closing: children
//! keep running as independent sessions, and the next open offers to resume from what the
//! version records. Everything here is pure; the review surface runs it (see
//! `surface/guide_fanout.rs`).

use bcode_code_review_models::{
    ClaimStatus, GenerationArea, GenerationState, GuideAreaStatus, GuideStatus, GuideStrategy,
    GuideVersion, Section, all_sections,
};
use bmux_tui::text::{Line, Span};
use std::collections::BTreeMap;

/// Area rows the panel and the card list at most; the rest are counted.
pub const MAX_PROGRESS_ROWS: usize = 4;
/// Area titles a confirmation names at most.
const MAX_NAMED_AREAS: usize = 3;

/// What a fan-out about to start would cost, as the reviewer is asked to confirm it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// Child agents.
    pub children: usize,
    /// Estimated tokens in all.
    pub tokens: u64,
    /// Estimated cost, formatted (`≈$1.05`), when the model's price is known.
    pub cost: Option<String>,
    /// The model the children run.
    pub model: String,
}

impl Estimate {
    /// `Fan out: 5 agents · ~350k tok · ≈$1.05 on <model>`.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!(
            "Fan out: {} agent{} · ~{} tok",
            self.children,
            if self.children == 1 { "" } else { "s" },
            short_tokens(self.tokens)
        );
        if let Some(cost) = &self.cost {
            text.push_str(" · ");
            text.push_str(cost);
        }
        if !self.model.is_empty() {
            text.push_str(" on ");
            text.push_str(&self.model);
        }
        text
    }
}

/// Tokens briefly: `950`, `12k`, `1.2M`.
#[must_use]
pub fn short_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{}k", tokens / 1_000),
        _ => format!("{}.{}M", tokens / 1_000_000, tokens % 1_000_000 / 100_000),
    }
}

/// The cost of `tokens` at an input price per million tokens formatted like `$3.00`; `None`
/// when the price is not in dollars.
#[must_use]
pub fn cost_of(tokens: u64, price_per_million: &str) -> Option<String> {
    let amount = price_per_million.trim().strip_prefix('$')?;
    let (dollars, cents) = amount.split_once('.').unwrap_or((amount, "0"));
    let cents_per_million = dollars.parse::<u64>().ok()? * 100
        + format!("{cents:0<2}").get(..2)?.parse::<u64>().ok()?;
    let cents = (tokens.saturating_mul(cents_per_million)).div_ceil(1_000_000);
    Some(format!("≈${}.{:02}", cents / 100, cents % 100))
}

/// The areas to start now: queued ones, in plan order, while fewer than `max_parallel` run.
#[must_use]
pub fn to_start(generation: &GenerationState) -> Vec<String> {
    let running = generation
        .areas
        .iter()
        .filter(|area| area.status == GuideAreaStatus::Running)
        .count();
    let slots = generation
        .max_parallel
        .unwrap_or(usize::MAX)
        .max(1)
        .saturating_sub(running);
    generation
        .areas
        .iter()
        .filter(|area| area.status == GuideAreaStatus::Queued)
        .take(slots)
        .map(|area| area.area_id.clone())
        .collect()
}

/// Whether the children have used up the soft token budget.
#[must_use]
pub fn over_budget(generation: &GenerationState) -> bool {
    generation
        .budget_tokens
        .is_some_and(|budget| generation.tokens_used >= budget)
}

/// Stop the queued areas because the budget ran out, with a note saying so; returns how many
/// were stopped.
pub fn stop_for_budget(generation: &mut GenerationState) -> usize {
    let mut stopped = 0;
    for area in &mut generation.areas {
        if area.status == GuideAreaStatus::Queued {
            area.status = GuideAreaStatus::Cancelled;
            area.message = Some("not started: the token budget ran out".to_owned());
            stopped += 1;
        }
    }
    if stopped > 0 {
        generation.note = Some(format!(
            "the token budget ({} tok) ran out: {stopped} area{} not written; the guide is \
             synthesized from what exists",
            short_tokens(generation.budget_tokens.unwrap_or_default()),
            if stopped == 1 { " was" } else { "s were" }
        ));
    }
    stopped
}

/// Sum the areas' tokens into the version's total.
pub fn total_tokens(generation: &mut GenerationState) {
    generation.tokens_used = generation.areas.iter().map(|area| area.tokens).sum();
}

/// Whether `version` is a fan-out that did not finish (an area is not written, or every area
/// is done but the guide was not synthesized), so an app not running it can resume it.
#[must_use]
pub const fn resumable(version: &GuideVersion) -> bool {
    let Some(generation) = &version.generation else {
        return false;
    };
    if generation.areas.is_empty() {
        return false;
    }
    !matches!(
        version.status,
        GuideStatus::Complete | GuideStatus::Superseded
    )
}

/// `4/5 areas ready · Resume`.
#[must_use]
pub fn resume_label(generation: &GenerationState, key: &str) -> String {
    let resume = if key.is_empty() {
        "Resume".to_owned()
    } else {
        format!("{key} Resume")
    };
    format!(
        "{}/{} {} ready · {resume}",
        generation.complete(),
        generation.areas.len(),
        if generation.areas.len() == 1 {
            "area"
        } else {
            "areas"
        }
    )
}

/// Make a stopped or interrupted fan-out runnable again: areas not written are queued (their
/// sessions forgotten, so new children write them).
pub fn requeue(generation: &mut GenerationState) {
    for area in &mut generation.areas {
        if area.status != GuideAreaStatus::Complete {
            area.status = GuideAreaStatus::Queued;
            area.session_id = None;
            area.message = None;
        }
    }
    generation.note = None;
}

/// Stop every area not done.
pub fn cancel(generation: &mut GenerationState) {
    for area in &mut generation.areas {
        if !area.status.is_terminal() {
            area.status = GuideAreaStatus::Cancelled;
            area.message = Some("stopped".to_owned());
        }
    }
}

/// What a child is asked: write `area` of version `version` of `guide_id`.
#[must_use]
pub fn area_prompt(
    guide_id: &str,
    version: u32,
    area: &GenerationArea,
    directions: Option<&str>,
    depth: &str,
) -> String {
    let mut prompt = format!(
        "Write the area {area_id} ({title}) of guide {guide_id}, version {version}, at {depth} \
         depth. It covers: {paths}.\n\nRead the area's changes (code_review.list_files, \
         code_review.file_diff, code_review.diff_hunks), then send one code_review.guide.write \
         with guide_id {guide_id}, version {version} and a single area_result operation \
         (area_id {area_id}) holding a short purpose section and one step section per stop in \
         the diff, citing exact lines with quotes. Keep section ids unique to the area (start \
         them with a1, a2, ...).",
        area_id = area.area_id,
        title = area.title,
        paths = if area.paths.is_empty() {
            "the files named by the area".to_owned()
        } else {
            area.paths.join(", ")
        },
    );
    push_directions(&mut prompt, directions);
    prompt
}

/// What a child is asked: write the layer guide `layer_guide` of `area`'s stack layer.
#[must_use]
pub fn layer_prompt(
    layer_guide: &str,
    area: &GenerationArea,
    directions: Option<&str>,
    depth: &str,
) -> String {
    let mut prompt = format!(
        "Write the guide of the stack layer {title} (source {source}): guide {layer_guide}, \
         version 1, at {depth} depth. It covers only this layer's own changes.\n\nRead the \
         layer's changes (code_review.list_files and code_review.file_diff with source \
         {source}), then write the guide with code_review.guide.write (guide_id {layer_guide}): \
         set_outline with purpose, scope and reading_order (one step per stop in the diff), \
         put_section per section citing exact lines with quotes, and set_status complete.",
        title = area.title,
        source = area.layer.as_deref().unwrap_or(&area.area_id),
    );
    push_directions(&mut prompt, directions);
    prompt
}

fn push_directions(prompt: &mut String, directions: Option<&str>) {
    if let Some(directions) = directions.map(str::trim).filter(|text| !text.is_empty()) {
        prompt.push_str("\n\nThe reviewer's directions: ");
        prompt.push_str(directions);
    }
}

/// What the parent is asked once every area is done: synthesize the guide from them.
#[must_use]
pub fn synthesis_prompt(
    guide_id: &str,
    version: u32,
    strategy: GuideStrategy,
    generation: &GenerationState,
) -> String {
    let finished: Vec<String> = generation
        .areas
        .iter()
        .map(|area| format!("{} ({})", area.title, area.status.as_str()))
        .collect();
    let note = generation
        .note
        .as_deref()
        .map(|note| format!("\n\nNote: {note}; say so in the scope section."))
        .unwrap_or_default();
    match strategy {
        GuideStrategy::PerLayer => format!(
            "The layer guides are written: {finished}. Write the stack guide {guide_id} \
             (version {version}): read each layer guide with code_review.guide.get, then write \
             purpose, scope and a reading order across the layers with code_review.guide.write \
             (the Layers section linking each layer guide is already there; do not remove it), \
             and set_status complete.{note}",
            finished = finished.join(", "),
        ),
        _ => format!(
            "Areas finished: {finished}. Synthesize guide {guide_id} (version {version}) from \
             them: read them with code_review.guide.get (guide_id {guide_id}, include \
             [\"outline\", \"bodies\", \"citations\", \"areas\"]), then write the guide with \
             code_review.guide.write: purpose, scope and one reading order across the areas, \
             citing the areas' citations by their ids (such as \"crates-net.c1\") instead of \
             citing again, and set_status complete.{note}",
            finished = finished.join(", "),
        ),
    }
}

/// What the verifier child is asked.
#[must_use]
pub fn verifier_prompt(guide_id: &str, version: u32) -> String {
    format!(
        "Check guide {guide_id}, version {version}: read it with code_review.guide.get \
         (include [\"outline\", \"bodies\", \"citations\"]) and record one check per claim of \
         every written section with code_review.guide.write record_checks (guide_id \
         {guide_id}, version {version}). Never change the content."
    )
}

/// Sections of `version` with a claim the verifier found unsupported, in reading order.
#[must_use]
pub fn unsupported_sections(version: &GuideVersion) -> Vec<String> {
    all_sections(&version.outline)
        .into_iter()
        .filter(|section: &&Section| {
            section
                .checks
                .iter()
                .any(|check| check.status == ClaimStatus::Unsupported)
        })
        .map(|section| section.id.clone())
        .collect()
}

/// What the writer is asked when the verifier found unsupported claims.
#[must_use]
pub fn refresh_prompt(sections: &[String]) -> String {
    format!(
        "The verifier found claims its citations do not support in {}. Read its checks with \
         code_review.guide.get and fix those claims (rewrite the claim, or cite the lines that \
         support it); change nothing else.",
        sections.join(", ")
    )
}

/// How one area shows in the panel and the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaRow {
    /// Title.
    pub title: String,
    /// Where it stands.
    pub status: GuideAreaStatus,
    /// Tokens its child used.
    pub tokens: u64,
}

/// A fan-out as the panel and the card show it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FanOutView {
    /// Areas listed (at most [`MAX_PROGRESS_ROWS`], running and queued first).
    pub areas: Vec<AreaRow>,
    /// Areas not listed.
    pub more: usize,
    /// Areas done.
    pub done: usize,
    /// Areas in all.
    pub total: usize,
    /// Tokens used in all.
    pub tokens: u64,
    /// What the fan-out does now (`combining areas`, `checking claims`), or a note.
    pub phase: Option<String>,
    /// A fan-out waiting for confirmation: the estimate and the keys that answer.
    pub confirm: Option<String>,
    /// A resumable fan-out: `4/5 areas ready · Resume`.
    pub resume: Option<String>,
    /// A child waiting for a permission answer: `crates/net wants code_review.search_code`.
    pub permission: Option<String>,
}

impl FanOutView {
    /// The view of `generation`, with each area's live tokens (by session) where known.
    #[must_use]
    pub fn of(generation: &GenerationState, live: &BTreeMap<String, u64>) -> Self {
        let tokens_of = |area: &GenerationArea| {
            area.session_id
                .as_ref()
                .and_then(|session| live.get(session))
                .copied()
                .unwrap_or(area.tokens)
                .max(area.tokens)
        };
        let mut rows: Vec<(usize, AreaRow)> = generation
            .areas
            .iter()
            .map(|area| {
                let rank = match area.status {
                    GuideAreaStatus::Running => 0,
                    GuideAreaStatus::Queued => 1,
                    GuideAreaStatus::Failed => 2,
                    GuideAreaStatus::Complete | GuideAreaStatus::Cancelled => 3,
                };
                (
                    rank,
                    AreaRow {
                        title: area.title.clone(),
                        status: area.status,
                        tokens: tokens_of(area),
                    },
                )
            })
            .collect();
        rows.sort_by_key(|(rank, _)| *rank);
        let total = rows.len();
        let tokens = rows.iter().map(|(_, row)| row.tokens).sum();
        let areas: Vec<AreaRow> = rows
            .into_iter()
            .take(MAX_PROGRESS_ROWS)
            .map(|(_, row)| row)
            .collect();
        Self {
            more: total - areas.len(),
            areas,
            done: generation.terminal(),
            total,
            tokens,
            phase: generation.note.clone(),
            ..Self::default()
        }
    }

    /// The summary line: `areas 3/4 · 182k tok`, then the phase.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = format!(
            "areas {}/{} · {} tok",
            self.done,
            self.total,
            short_tokens(self.tokens)
        );
        if let Some(phase) = &self.phase {
            text.push_str(" · ");
            text.push_str(phase);
        }
        text
    }

    /// Names of up to [`MAX_NAMED_AREAS`] areas, for a confirmation.
    #[must_use]
    pub fn named(generation: &GenerationState) -> String {
        let mut names: Vec<&str> = generation
            .areas
            .iter()
            .take(MAX_NAMED_AREAS)
            .map(|area| area.title.as_str())
            .collect();
        if generation.areas.len() > MAX_NAMED_AREAS {
            names.push("…");
        }
        names.join(", ")
    }
}

/// Status word of an area row.
#[must_use]
pub const fn status_word(status: GuideAreaStatus) -> &'static str {
    match status {
        GuideAreaStatus::Queued => "queued",
        GuideAreaStatus::Running => "writing",
        GuideAreaStatus::Complete => "ready",
        GuideAreaStatus::Failed => "failed",
        GuideAreaStatus::Cancelled => "stopped",
    }
}

/// The rows the guide panel shows for `view` under its banner.
///
/// That is a confirmation alone, else the summary, a child's permission request (with the feed
/// keys that answer it), the listed areas, and a resume offer. Each row is its text and
/// right-aligned trailing text.
#[must_use]
pub fn progress_rows(
    view: &FanOutView,
    theme: &crate::theme::ReviewTheme,
    feed_keys: &str,
) -> Vec<(Line, Line)> {
    let icons = &theme.icons;
    let row = |glyph: &'static str, style, text: String| {
        Line::from_spans(vec![
            Span::styled(glyph, style),
            Span::raw(" "),
            Span::styled(text, style),
        ])
    };
    if let Some(confirm) = &view.confirm {
        return vec![(
            row(icons.alert, theme.accent, confirm.clone()),
            Line::default(),
        )];
    }
    let mut rows = Vec::new();
    if view.total > 0 {
        rows.push((
            row(icons.running, theme.muted, view.summary()),
            Line::default(),
        ));
    }
    if let Some(permission) = &view.permission {
        let text = if feed_keys.is_empty() {
            permission.clone()
        } else {
            format!("{feed_keys}  {permission}")
        };
        rows.push((row(icons.alert, theme.guide.problem, text), Line::default()));
    }
    for area in &view.areas {
        let (glyph, style) = match area.status {
            GuideAreaStatus::Queued | GuideAreaStatus::Running => (icons.running, theme.accent),
            GuideAreaStatus::Complete => (icons.viewed, theme.viewed),
            GuideAreaStatus::Failed => (icons.failed, theme.error),
            GuideAreaStatus::Cancelled => (icons.unviewed, theme.muted),
        };
        let trailing = if area.tokens > 0 {
            format!(
                "{} · {} tok",
                status_word(area.status),
                short_tokens(area.tokens)
            )
        } else {
            status_word(area.status).to_owned()
        };
        rows.push((
            Line::from_spans(vec![
                Span::raw("  "),
                Span::styled(glyph, style),
                Span::raw(" "),
                Span::styled(area.title.clone(), theme.text),
            ]),
            Line::from_spans(vec![Span::styled(trailing, theme.muted)]),
        ));
    }
    if view.more > 0 {
        rows.push((
            Line::from_spans(vec![Span::styled(
                format!(
                    "  +{} more area{}",
                    view.more,
                    if view.more == 1 { "" } else { "s" }
                ),
                theme.tiers.dim,
            )]),
            Line::default(),
        ));
    }
    if let Some(resume) = &view.resume {
        rows.push((
            row(icons.guide_marker, theme.accent, resume.clone()),
            Line::default(),
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(id: &str, status: GuideAreaStatus) -> GenerationArea {
        GenerationArea {
            area_id: id.to_owned(),
            title: id.to_owned(),
            session_id: None,
            status,
            paths: vec![id.to_owned()],
            layer: None,
            guide_id: None,
            tokens: 0,
            message: None,
        }
    }

    fn generation(statuses: &[GuideAreaStatus], max_parallel: usize) -> GenerationState {
        GenerationState {
            areas: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| area(&format!("a{index}"), *status))
                .collect(),
            max_parallel: Some(max_parallel),
            ..GenerationState::default()
        }
    }

    #[test]
    fn at_most_max_parallel_children_run_and_the_rest_wait_in_order() {
        use GuideAreaStatus::{Complete, Queued, Running};
        let state = generation(&[Running, Queued, Queued, Queued, Complete], 2);
        assert_eq!(to_start(&state), vec!["a1".to_owned()]);
        let state = generation(&[Running, Running, Queued], 2);
        assert_eq!(to_start(&state), Vec::<String>::new());
        let state = generation(&[Queued, Queued, Queued], 4);
        assert_eq!(to_start(&state).len(), 3);
    }

    #[test]
    fn the_budget_stops_queued_areas_with_a_note_and_keeps_written_ones() {
        use GuideAreaStatus::{Complete, Queued, Running};
        let mut state = generation(&[Complete, Running, Queued, Queued], 1);
        state.budget_tokens = Some(1_000);
        state.areas[0].tokens = 600;
        state.areas[1].tokens = 500;
        total_tokens(&mut state);
        assert!(over_budget(&state));
        assert_eq!(stop_for_budget(&mut state), 2);
        assert_eq!(state.areas[0].status, Complete);
        assert_eq!(state.areas[1].status, Running);
        assert!(
            state
                .note
                .as_deref()
                .is_some_and(|note| note.contains("2 areas"))
        );
    }

    #[test]
    fn a_stopped_fan_out_resumes_its_missing_areas_only() {
        use GuideAreaStatus::{Cancelled, Complete, Failed, Running};
        let mut state = generation(&[Complete, Running, Failed, Cancelled, Complete], 2);
        state.areas[1].session_id = Some("s".to_owned());
        assert_eq!(resume_label(&state, "f"), "2/5 areas ready · f Resume");
        requeue(&mut state);
        assert_eq!(to_start(&state), vec!["a1".to_owned(), "a2".to_owned()]);
        assert_eq!(state.areas[1].session_id, None);
        assert_eq!(state.areas[0].status, Complete);
    }

    #[test]
    fn estimates_name_children_tokens_cost_and_model() {
        let estimate = Estimate {
            children: 5,
            tokens: 350_000,
            cost: cost_of(350_000, "$3.00"),
            model: "Sonnet".to_owned(),
        };
        assert_eq!(
            estimate.text(),
            "Fan out: 5 agents · ~350k tok · ≈$1.05 on Sonnet"
        );
        assert_eq!(cost_of(1_000_000, "12.00 EUR"), None);
        assert_eq!(cost_of(500_000, "$0.5"), Some("≈$0.25".to_owned()));
    }

    #[test]
    fn the_view_lists_running_areas_first_with_live_tokens_and_counts_the_rest() {
        use GuideAreaStatus::{Complete, Queued, Running};
        let mut state = generation(&[Complete, Queued, Running, Complete, Complete, Queued], 2);
        state.areas[2].session_id = Some("child".to_owned());
        let live = BTreeMap::from([("child".to_owned(), 42_000)]);
        let view = FanOutView::of(&state, &live);
        assert_eq!(view.areas[0].status, Running);
        assert_eq!(view.areas[0].tokens, 42_000);
        assert_eq!(view.areas.len(), MAX_PROGRESS_ROWS);
        assert_eq!(view.more, 2);
        assert_eq!(view.summary(), "areas 3/6 · 42k tok");
    }
}
