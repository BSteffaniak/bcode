//! Repository read benchmark on the benchmark fixture.
//!
//! Run with:
//!
//! ```text
//! cargo test -p bcode_code_review data_layer_benchmark -- --ignored --nocapture
//! ```

use crate::data::ReviewData;
use crate::testing::fixture::{FixtureSpec, GitFixture, LARGE_FILE};
use bcode_code_review_models::{
    REVIEW_INDEX_MAX_PAGE_SIZE, ResolveReviewSourcesRequest, ResolvedSource,
    ReviewBlobLinesRequest, ReviewFileHunksRequest, ReviewIndexEntry, ReviewIndexRequest,
    ReviewIndexResponse, ReviewSource, ReviewSourceKind,
};
use std::path::Path;
use std::time::{Duration, Instant};

/// Ordinary modified files whose hunks are loaded one by one.
const HUNK_SAMPLES: usize = 200;
/// Context expansions (20-line blob reads) sampled.
const BLOB_SAMPLES: usize = 200;

fn ms(duration: Duration) -> String {
    format!("{:.2} ms", duration.as_secs_f64() * 1_000.0)
}

fn kib(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / 1024.0;
    format!("{value:.1} KiB ({bytes} bytes)")
}

fn stats(mut samples: Vec<Duration>) -> String {
    samples.sort_unstable();
    let count = u32::try_from(samples.len()).expect("sample count").max(1);
    let total: Duration = samples.iter().sum();
    let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
    format!(
        "avg {} / p95 {} / max {} ({} samples)",
        ms(total / count),
        ms(p95),
        ms(*samples.last().expect("samples")),
        samples.len()
    )
}

fn report(rows: &mut Vec<(String, String)>, label: impl Into<String>, value: impl Into<String>) {
    let (label, value) = (label.into(), value.into());
    eprintln!("{label}: {value}");
    rows.push((label, value));
}

fn resolve(service: &ReviewData, repo: &Path, kind: ReviewSourceKind) -> ResolvedSource {
    service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![ReviewSource {
                id: "source-1".to_string(),
                label: kind.label(),
                kind,
                included: true,
            }],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve")
        .sources
        .remove(0)
}

/// Load every page of the index; return entries, total wall time, and total JSON payload size.
fn full_index(
    service: &ReviewData,
    repo: &Path,
    source: &ResolvedSource,
    limit: u32,
) -> (Vec<ReviewIndexEntry>, Duration, usize, usize) {
    let mut entries = Vec::new();
    let mut cursor = None;
    let mut elapsed = Duration::ZERO;
    let mut bytes = 0;
    let mut first_page_bytes = 0;
    loop {
        let started = Instant::now();
        let page = service
            .index_get(&ReviewIndexRequest {
                repo_path: repo.to_path_buf(),
                source: source.clone(),
                cursor,
                limit: Some(limit),
            })
            .expect("index page");
        let payload = serde_json::to_vec(&page).expect("encode");
        let decoded: ReviewIndexResponse = serde_json::from_slice(&payload).expect("decode");
        elapsed += started.elapsed();
        if first_page_bytes == 0 {
            first_page_bytes = payload.len();
        }
        bytes += payload.len();
        entries.extend(decoded.entries);
        cursor = decoded.next_cursor;
        if cursor.is_none() {
            return (entries, elapsed, bytes, first_page_bytes);
        }
    }
}

fn measure_index(
    rows: &mut Vec<(String, String)>,
    label: &str,
    service: &ReviewData,
    repo: &Path,
    source: &ResolvedSource,
) -> Vec<ReviewIndexEntry> {
    let (entries, cold, bytes, _) = full_index(service, repo, source, REVIEW_INDEX_MAX_PAGE_SIZE);
    report(
        rows,
        format!("{label}: review.index.get, all pages (5,000/page, incl. JSON encode+decode)"),
        format!("{} for {} entries", ms(cold), entries.len()),
    );
    report(
        rows,
        format!("{label}: index JSON payload, all pages"),
        kib(bytes),
    );
    let started = Instant::now();
    let (_, _, _, first_page) = full_index(service, repo, source, 1_000);
    report(
        rows,
        format!("{label}: index, all pages at 1,000/page (warm)"),
        ms(started.elapsed()),
    );
    report(
        rows,
        format!("{label}: index first page (1,000 entries)"),
        kib(first_page),
    );
    entries
}

/// Per-file hunk loads for ordinary files and the 40k-line file.
fn measure_hunks(
    rows: &mut Vec<(String, String)>,
    service: &ReviewData,
    repo: &Path,
    branch: &ResolvedSource,
    entries: &[ReviewIndexEntry],
) {
    let mut samples = Vec::new();
    let mut payload = 0;
    for entry in entries
        .iter()
        .filter(|entry| entry.path.starts_with("src/mod_"))
        .take(HUNK_SAMPLES)
    {
        let started = Instant::now();
        let response = service
            .file_hunks_get(&ReviewFileHunksRequest {
                repo_path: repo.to_path_buf(),
                source: branch.clone(),
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
                context_lines: None,
                ignore_whitespace: false,
                max_lines: None,
            })
            .expect("hunks");
        payload += serde_json::to_vec(&response).expect("encode").len();
        samples.push(started.elapsed());
    }
    report(
        rows,
        "branch compare: review.file.hunks.get, ordinary file (10 hunks, 100 changed lines)",
        stats(samples),
    );
    report(
        rows,
        "branch compare: hunks JSON payload per ordinary file (avg)",
        kib(payload / HUNK_SAMPLES),
    );

    for (label, max_lines) in [
        ("default 20,000-line budget", None),
        ("untruncated", Some(200_000)),
    ] {
        let started = Instant::now();
        let response = service
            .file_hunks_get(&ReviewFileHunksRequest {
                repo_path: repo.to_path_buf(),
                source: branch.clone(),
                path: LARGE_FILE.to_string(),
                old_path: None,
                context_lines: None,
                ignore_whitespace: false,
                max_lines,
            })
            .expect("large hunks");
        let elapsed = started.elapsed();
        let lines: usize = response.hunks.iter().map(|hunk| hunk.lines.len()).sum();
        let bytes = serde_json::to_vec(&response).expect("encode").len();
        report(
            rows,
            format!("branch compare: 40k-line file hunks, {label}"),
            format!("{} for {lines} lines, {}", ms(elapsed), kib(bytes)),
        );
    }
}

/// Context expansion reads (20 lines) by revision and by object id.
fn measure_blob_lines(
    rows: &mut Vec<(String, String)>,
    service: &ReviewData,
    repo: &Path,
    branch: &ResolvedSource,
    entries: &[ReviewIndexEntry],
) {
    let large = entries
        .iter()
        .find(|entry| entry.path == LARGE_FILE)
        .expect("large file entry");
    let oid = large.new_blob.oid().expect("large blob").to_string();
    let mut samples = Vec::new();
    for index in 0..BLOB_SAMPLES {
        let started = Instant::now();
        service
            .blob_lines_get(&ReviewBlobLinesRequest {
                repo_path: repo.to_path_buf(),
                blob: bcode_code_review_models::ReviewBlobSelector::Revision {
                    revision: branch.head.clone(),
                    path: LARGE_FILE.to_string(),
                },
                start_line: u32::try_from(index * 150 + 1).expect("line"),
                count: 20,
            })
            .expect("blob lines");
        samples.push(started.elapsed());
    }
    report(
        rows,
        "review.blob.lines.get, 20 lines of the 40k-line file by revision (cat-file each time)",
        stats(samples),
    );
    let mut samples = Vec::new();
    for index in 0..BLOB_SAMPLES {
        let started = Instant::now();
        service
            .blob_lines_get(&ReviewBlobLinesRequest {
                repo_path: repo.to_path_buf(),
                blob: bcode_code_review_models::ReviewBlobSelector::Object { oid: oid.clone() },
                start_line: u32::try_from(index * 150 + 1).expect("line"),
                count: 20,
            })
            .expect("blob lines");
        samples.push(started.elapsed());
    }
    report(
        rows,
        "review.blob.lines.get, 20 lines of the 40k-line file by object id (cached)",
        stats(samples),
    );
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn data_layer_benchmark() {
    let mut rows = Vec::new();
    let started = Instant::now();
    let fixture = GitFixture::generate(FixtureSpec::benchmark());
    report(&mut rows, "fixture generation", ms(started.elapsed()));
    let repo = fixture.root();
    let service = ReviewData::default();

    let started = Instant::now();
    let branch = resolve(
        &service,
        repo,
        crate::testing::fixture_branch_compare().into(),
    );
    report(
        &mut rows,
        "branch compare: review.sources.resolve",
        ms(started.elapsed()),
    );
    let entries = measure_index(&mut rows, "branch compare", &service, repo, &branch);

    measure_hunks(&mut rows, &service, repo, &branch, &entries);
    measure_blob_lines(&mut rows, &service, repo, &branch, &entries);

    let worktree = resolve(&service, repo, ReviewSourceKind::WorkingTreeAndIndex);
    measure_index(&mut rows, "working tree + index", &service, repo, &worktree);
    let started = Instant::now();
    service
        .sources_token(&bcode_code_review_models::ReviewSourcesTokenRequest {
            repo_path: repo.to_path_buf(),
            include_working_tree: true,
        })
        .expect("token");
    report(
        &mut rows,
        "review.sources.token (with working tree)",
        ms(started.elapsed()),
    );

    let profile = if cfg!(debug_assertions) {
        "debug (test profile)"
    } else {
        "release"
    };
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    println!("\nData layer ({profile}, fixture {:?})", fixture.spec());
    println!("{:-<1$}", "", width + 40);
    for (label, value) in &rows {
        println!("{label:<width$}  {value}");
    }
}
