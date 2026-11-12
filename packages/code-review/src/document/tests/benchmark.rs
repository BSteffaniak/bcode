//! Review document micro-benchmark on the benchmark fixture index.
//!
//! Run with:
//!
//! ```text
//! cargo test --release -p bcode_code_review document_benchmark -- --ignored --nocapture
//! ```
//!
//! The document is built from the real 5,063-entry branch-compare index. Scrolling then pages
//! through the whole review; every hunk and blob request the document issues is answered at
//! once with synthetic content shaped by the entry's `+/-` counts, so the numbers measure the
//! document (needs, item construction, splices, residency), not git. The viewport is kept
//! anchored through splices, as a virtual list does, so every file is visited.

use super::super::{DocumentConfig, DocumentNeed, ReviewDocument, SourceInput};
use super::support::{Edit, SynthFile};
use crate::ReviewData;
use crate::testing::fixture::{FixtureSpec, GitFixture};
use bcode_code_review_models::{
    REVIEW_INDEX_MAX_PAGE_SIZE, ResolveReviewSourcesRequest, ReviewBlobLinesResponse,
    ReviewFileStatus, ReviewIndexEntry, ReviewIndexRequest, ReviewSource,
    ReviewStackAttributionResponse,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Items visible per page.
const PAGE_ITEMS: usize = 60;

fn ms(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
}

fn synth(entry: &ReviewIndexEntry) -> SynthFile {
    match entry.status {
        ReviewFileStatus::Added => SynthFile::added(&entry.path, entry.additions.max(1)),
        ReviewFileStatus::Deleted => SynthFile::deleted(&entry.path, entry.deletions.max(1)),
        _ => {
            // Replace five lines per edit, twenty lines apart, until the counts are covered.
            let groups = entry.additions.max(entry.deletions).div_ceil(5).max(1);
            let lines = (groups * 20).max(200);
            let edits: Vec<Edit> = (0..groups)
                .map(|group| Edit {
                    at: group * 20 + 8,
                    remove: 5,
                    add: 5,
                })
                .collect();
            SynthFile::modified(&entry.path, lines, &edits, 3)
        }
    }
}

fn answer(doc: &mut ReviewDocument, cache: &mut BTreeMap<String, SynthFile>, need: &DocumentNeed) {
    let path = need.file().path.clone();
    let entry = doc.entry(need.file()).expect("entry").clone();
    let file = cache.entry(path.clone()).or_insert_with(|| synth(&entry));
    match need {
        DocumentNeed::Hunks { ticket, .. } => {
            doc.accept_hunks(*ticket, &file.hunks_response(&entry.source_id));
        }
        DocumentNeed::BlobLines {
            ticket, request, ..
        } => {
            let lines = file.new.as_ref().or(file.old.as_ref()).expect("side");
            let first = (request.start_line.max(1) - 1) as usize;
            let end = (first + request.count as usize).min(lines.len());
            doc.accept_blob_lines(
                *ticket,
                &ReviewBlobLinesResponse {
                    start_line: request.start_line.max(1),
                    lines: lines
                        .get(first..end)
                        .map(<[String]>::to_vec)
                        .unwrap_or_default(),
                    total_lines: u32::try_from(lines.len()).expect("fits"),
                    binary: false,
                },
            );
        }
        DocumentNeed::Attribution { ticket, .. } => {
            doc.accept_attribution(
                *ticket,
                ReviewStackAttributionResponse {
                    path,
                    new_ranges: Vec::new(),
                    old_ranges: Vec::new(),
                    binary: false,
                },
            );
        }
    }
}

fn stats(label: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let count = u32::try_from(samples.len()).expect("count").max(1);
    let total: Duration = samples.iter().sum();
    let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
    println!(
        "{label}: avg {} / p95 {} / max {} ({} samples)",
        ms(total / count),
        ms(p95),
        ms(*samples.last().expect("samples")),
        samples.len()
    );
}

/// Resolve the fixture's branch compare and load its whole index.
fn branch_compare_index(
    repo: &std::path::Path,
) -> (
    bcode_code_review_models::ResolvedSource,
    Vec<ReviewIndexEntry>,
) {
    let mut service = ReviewData::default();
    let kind: bcode_code_review_models::ReviewSourceKind =
        crate::testing::fixture_branch_compare().into();
    let source = service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![ReviewSource {
                id: "source-1".into(),
                label: kind.label(),
                kind,
                included: true,
            }],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve")
        .sources
        .remove(0);
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = service
            .index_get(&ReviewIndexRequest {
                repo_path: repo.to_path_buf(),
                source: source.clone(),
                cursor,
                limit: Some(REVIEW_INDEX_MAX_PAGE_SIZE),
            })
            .expect("index");
        entries.extend(page.entries);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    (source, entries)
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn document_benchmark() {
    let fixture = GitFixture::generate(FixtureSpec::benchmark());
    let repo = fixture.root();
    let (source, entries) = branch_compare_index(repo);
    let file_count = entries.len();

    let started = Instant::now();
    let mut doc = ReviewDocument::new(
        repo.to_path_buf(),
        vec![SourceInput {
            source,
            label: "branch compare".into(),
            entries,
            stack: None,
        }],
        DocumentConfig::default(),
    );
    let build = started.elapsed();
    let initial_items = doc.len();

    let mut cache = BTreeMap::new();
    let mut steps = Vec::new();
    let mut reads = Vec::new();
    let mut splices = 0_usize;
    let mut loads = 0_usize;
    let mut peak_bytes = 0;
    let mut first = 0;
    let scroll_started = Instant::now();
    while first < doc.len() {
        let started = Instant::now();
        doc.set_viewport(first, first + PAGE_ITEMS - 1);
        let needs = doc.take_needs();
        loads += needs
            .iter()
            .filter(|need| matches!(need, DocumentNeed::Hunks { .. }))
            .count();
        for need in &needs {
            answer(&mut doc, &mut cache, need);
        }
        // Keep the synthetic content bounded like the document's own residency.
        if cache.len() > 256 {
            cache.clear();
        }
        // Keep the viewport anchored like a virtual list: splices entirely above the viewport
        // shift it, so content collapsing above (evictions) is not skipped.
        for splice in doc.take_splices() {
            splices += 1;
            if splice.start + splice.removed <= first {
                first = first + splice.inserted - splice.removed;
            }
        }
        steps.push(started.elapsed());
        let started = Instant::now();
        for index in first..(first + PAGE_ITEMS).min(doc.len()) {
            let key = doc.item(index).map(super::super::ReviewItem::key);
            std::hint::black_box(doc.sections(index));
            // Anchoring resolves the anchor item's key every frame.
            let resolved = key.and_then(|key| doc.index_of(&key));
            assert_eq!(resolved, Some(index));
        }
        reads.push(started.elapsed());
        peak_bytes = peak_bytes.max(doc.resident_bytes());
        first += PAGE_ITEMS;
    }
    let scroll = scroll_started.elapsed();
    let resident_files = doc.resident_files().count();
    assert!(loads >= file_count, "every file was loaded at least once");

    let profile = if cfg!(debug_assertions) {
        "debug (test profile)"
    } else {
        "release"
    };
    println!("\nReview document ({profile}), {file_count} files");
    println!("build document: {} ({initial_items} items)", ms(build));
    println!(
        "scroll whole review: {} for {} pages of {PAGE_ITEMS} items, final {} items",
        ms(scroll),
        steps.len(),
        doc.len()
    );
    stats("page step (viewport + needs + loads + splices)", steps);
    stats(
        "page read (item, sections, and key → index for 60 items)",
        reads,
    );
    println!(
        "hunk loads {loads}, splices {splices}, peak resident {:.1} MiB (budget {} MiB), resident files at end {resident_files}",
        f64::from(u32::try_from(peak_bytes / 1024).unwrap_or(u32::MAX)) / 1024.0,
        DocumentConfig::default().residency_budget_bytes / (1024 * 1024),
    );
    assert!(peak_bytes <= DocumentConfig::default().residency_budget_bytes * 2);
}
