//! Timing harness for per-item presentation resolution.
//!
//! Run with
//! `cargo test -p bcode_tui --release structure::bench -- --ignored --nocapture`.
//!
//! It builds a synthetic 5,000-item transcript, compiles a bundled profile's
//! rules, and times the work rendering does with that snapshot:
//!
//! * the signature scan every incremental layout sync performs over all
//!   items (the cost of a disclosure toggle frame);
//! * the presentation resolution inside that scan, split into fact
//!   extraction and policy matching;
//! * a full relayout (a width change), with the number of unit resolutions
//!   it performs, so the share spent resolving can be estimated.

use std::cell::Cell;
use std::hint::black_box;
use std::time::{Duration, Instant};

use bcode_presentation_policy::models::{LayerSource, PresentationSection, RuleLayer};
use bcode_session_view_models::TextFormat;
use bcode_tui_components::transcript::item::{
    TranscriptItem, permission_request_item, tool_result_item,
};

use super::{StructureSnapshot, StructureState};

thread_local! {
    static RESOLUTIONS: Cell<u64> = const { Cell::new(0) };
}

/// Record one unit resolution (called from the resolution paths under test).
pub(super) fn count_resolution() {
    RESOLUTIONS.with(|count| count.set(count.get().saturating_add(1)));
}

fn resolutions() -> u64 {
    RESOLUTIONS.with(Cell::get)
}

const ITEMS: usize = 5_000;
const ROUNDS: u32 = 20;
const TOOLS: [&str; 6] = ["read", "bash", "grep", "edit", "glob", "write"];

fn profile_layer(id: &str, source: &str) -> RuleLayer {
    let table: toml::Table = toml::from_str(source).unwrap();
    let presentation: PresentationSection = table
        .get("presentation")
        .cloned()
        .map(|value| value.try_into().unwrap())
        .unwrap_or_default();
    RuleLayer {
        source: LayerSource::Theme(id.to_owned()),
        rules: presentation.rules,
    }
}

fn synthetic_transcript() -> Vec<TranscriptItem> {
    (0..ITEMS)
        .map(|index| match index % 10 {
            0 => TranscriptItem::new("You", format!("request {index}")),
            1 => TranscriptItem::with_format(
                "Assistant",
                format!("Working on **step {index}**.\n\n- one\n- two\n"),
                TextFormat::Markdown,
            ),
            9 if index % 20 == 9 => permission_request_item(
                &format!("perm-{index}"),
                &format!("call-{index}"),
                "bash",
                "{\"command\":\"ls\"}",
                None,
                None,
            ),
            slot => {
                let tool = TOOLS[slot % TOOLS.len()];
                let arguments = format!("{{\"path\":\"src/file_{index}.rs\",\"command\":\"ls\"}}");
                let output = (0..12)
                    .map(|line| format!("line {line} of {tool} output {index}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                tool_result_item(
                    &format!("call-{index}"),
                    Some(tool),
                    Some(&arguments),
                    &output,
                    index % 37 == 0,
                )
            }
        })
        .collect()
}

fn time(rounds: u32, mut work: impl FnMut()) -> Duration {
    // One warm-up pass fills per-item memos the way earlier frames would.
    work();
    let started = Instant::now();
    for _ in 0..rounds {
        work();
    }
    started.elapsed() / rounds
}

fn snapshot(items: &[TranscriptItem]) -> StructureSnapshot {
    let mut state = StructureState::default();
    state.set_layers(&[
        profile_layer(
            "focus",
            include_str!("../../../tui-theme/themes/profiles/focus.toml"),
        ),
        profile_layer(
            "compact",
            include_str!("../../../tui-theme/themes/profiles/compact.toml"),
        ),
    ]);
    state.sync_groups(&items.iter().collect::<Vec<_>>());
    state.snapshot()
}

#[test]
#[ignore = "timing harness; run in release with --ignored --nocapture"]
fn resolution_cost_for_large_transcripts() {
    let items = synthetic_transcript();
    let snapshot = snapshot(&items);
    crate::render::set_structure(snapshot.clone());

    let scan = time(ROUNDS, || {
        for item in &items {
            black_box(crate::render::transcript_item_signature(item, 100, ()));
        }
    });
    let presentation = time(ROUNDS, || {
        for item in &items {
            black_box(snapshot.presentation(item));
        }
    });
    let facts = time(ROUNDS, || {
        for item in &items {
            black_box(snapshot.facts(item));
        }
    });
    let all_facts: Vec<_> = items
        .iter()
        .filter_map(|item| snapshot.facts(item))
        .collect();
    let policy = time(ROUNDS, || {
        for facts in &all_facts {
            black_box(snapshot.resolve(facts));
        }
    });

    let before = resolutions();
    let relayout_started = Instant::now();
    let rows = crate::render::sample_transcript_rows(&items, 100, snapshot.clone());
    let relayout = relayout_started.elapsed();
    let relayout_resolutions = resolutions() - before;
    black_box(rows);
    let relayout = time(3, || {
        black_box(crate::render::sample_transcript_rows(
            &items,
            100,
            snapshot.clone(),
        ));
    })
    .min(relayout);

    let per_resolution = presentation.as_secs_f64() / f64::from(u32::try_from(ITEMS).unwrap());
    let relayout_resolution_cost =
        per_resolution * f64::from(u32::try_from(relayout_resolutions).unwrap());
    eprintln!("items: {ITEMS}, compiled rules: {}", snapshot.policy.len());
    eprintln!("signature scan (toggle frame): {scan:?}");
    eprintln!(
        "  presentation resolution: {presentation:?} ({:.1}% of scan)",
        100.0 * presentation.as_secs_f64() / scan.as_secs_f64()
    );
    eprintln!("    fact extraction: {facts:?}");
    eprintln!("    policy matching: {policy:?}");
    eprintln!(
        "full relayout: {relayout:?}, unit resolutions: {relayout_resolutions} \
         (~{:.2} ms, {:.1}% of relayout)",
        relayout_resolution_cost * 1e3,
        100.0 * relayout_resolution_cost / relayout.as_secs_f64()
    );
}
