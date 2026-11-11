//! V1 baseline benchmark on the phase 0 fixture.
//!
//! Run with:
//!
//! ```text
//! cargo test -p bcode_code_review_plugin v1_baseline -- --ignored --nocapture
//! ```

use super::fixture::{FixtureSpec, GitFixture, LARGE_FILE};
use super::harness::Scenario;
use super::{fixture_branch_compare, materialize_v1};
use crate::code_review_tui::{
    CodeReviewSurface, ReviewApp, ReviewFile, review_summary_from_materialization,
};
use bcode_code_review_models::{MaterializeReviewWorkspaceResponse, ReviewTarget};
use bmux_tui::event::MouseEventKind;
use std::time::{Duration, Instant};

const WIDTH: u16 = 200;
const HEIGHT: u16 = 60;
/// Upper bound on sampled frames per interaction.
const MAX_FRAMES: usize = 200;
/// Lower bound on sampled frames per interaction, regardless of the time budget.
const MIN_FRAMES: usize = 10;
/// Wall-clock budget per interaction; V1 frames are slow enough that this usually binds.
const FRAME_BUDGET: Duration = Duration::from_secs(60);

struct Row {
    label: String,
    value: String,
}

fn row(label: impl Into<String>, value: impl Into<String>) -> Row {
    Row {
        label: label.into(),
        value: value.into(),
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.2} ms", duration.as_secs_f64() * 1_000.0)
}

fn mib(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / (1024.0 * 1024.0);
    format!("{value:.1} MiB ({bytes} bytes)")
}

fn frame_stats(label: &str, mut samples: Vec<Duration>) -> Vec<Row> {
    samples.sort_unstable();
    let total: Duration = samples.iter().sum();
    let count = u32::try_from(samples.len()).expect("sample count");
    let p95_index = (samples.len() * 95).div_ceil(100).saturating_sub(1);
    vec![
        row(format!("{label}: frames"), samples.len().to_string()),
        row(format!("{label}: average"), ms(total / count.max(1))),
        row(format!("{label}: p95"), ms(samples[p95_index])),
        row(
            format!("{label}: max"),
            ms(*samples.last().expect("samples")),
        ),
    ]
}

struct Materialized {
    response: MaterializeReviewWorkspaceResponse,
    rows: Vec<Row>,
}

fn measure_materialize(label: &str, fixture: &GitFixture, target: ReviewTarget) -> Materialized {
    let started = Instant::now();
    let response = materialize_v1(fixture.root(), target);
    let materialize = started.elapsed();

    let started = Instant::now();
    let payload = serde_json::to_vec(&response).expect("encode materialization");
    let encode = started.elapsed();

    let started = Instant::now();
    let decoded: MaterializeReviewWorkspaceResponse =
        serde_json::from_slice(&payload).expect("decode materialization");
    let decode = started.elapsed();

    let materialization = &decoded.materialization;
    let files = materialization
        .surfaces
        .iter()
        .filter(|surface| surface.file.is_some())
        .count();
    let rows = vec![
        row(format!("{label}: materialize"), ms(materialize)),
        row(format!("{label}: JSON payload"), mib(payload.len())),
        row(format!("{label}: JSON encode"), ms(encode)),
        row(format!("{label}: JSON decode"), ms(decode)),
        row(
            format!("{label}: diff files / +lines / -lines"),
            format!(
                "{files} / {} / {}",
                materialization.additions, materialization.deletions
            ),
        ),
        row(
            format!("{label}: repository files / branches / commits"),
            format!(
                "{} / {} / {}",
                materialization.repository_files.len(),
                materialization.repository_branches.len(),
                materialization.repository_commits.len()
            ),
        ),
    ];
    Materialized {
        response: decoded,
        rows,
    }
}

fn build_app(label: &str, response: MaterializeReviewWorkspaceResponse) -> (ReviewApp, Vec<Row>) {
    let started = Instant::now();
    let summary = review_summary_from_materialization(response);
    let convert = started.elapsed();
    let started = Instant::now();
    let app = ReviewApp::new(summary);
    let construct = started.elapsed();
    (
        app,
        vec![
            row(format!("{label}: summary conversion"), ms(convert)),
            row(format!("{label}: ReviewApp::new"), ms(construct)),
        ],
    )
}

fn file_path(file: &ReviewFile) -> Option<&str> {
    file.new_path.as_deref().or(file.old_path.as_deref())
}

fn timed_render(scenario: &mut Scenario<CodeReviewSurface>) -> Duration {
    scenario.settle();
    let started = Instant::now();
    let _screen = scenario.render();
    started.elapsed()
}

/// Per-frame samples of input handling and rendering for one interaction.
struct FrameSamples {
    input: Vec<Duration>,
    render: Vec<Duration>,
}

/// Repeat `input` then one frame until [`MAX_FRAMES`] frames or [`FRAME_BUDGET`] elapse,
/// whichever comes first (at least [`MIN_FRAMES`] frames).
fn sample_frames(
    scenario: &mut Scenario<CodeReviewSurface>,
    label: &str,
    mut input: impl FnMut(&mut Scenario<CodeReviewSurface>),
) -> FrameSamples {
    let mut samples = FrameSamples {
        input: Vec::new(),
        render: Vec::new(),
    };
    let started = Instant::now();
    while samples.render.len() < MAX_FRAMES
        && (samples.render.len() < MIN_FRAMES || started.elapsed() < FRAME_BUDGET)
    {
        let input_started = Instant::now();
        input(scenario);
        samples.input.push(input_started.elapsed());
        samples.render.push(timed_render(scenario));
        eprintln!(
            "  {label} frame {}: input {} render {}",
            samples.render.len(),
            ms(*samples.input.last().expect("input sample")),
            ms(*samples.render.last().expect("render sample"))
        );
    }
    samples
}

fn push(rows: &mut Vec<Row>, new_rows: impl IntoIterator<Item = Row>) {
    for row in new_rows {
        eprintln!("{}: {}", row.label, row.value);
        rows.push(row);
    }
}

/// Sample next-file navigation across ordinary modified files.
fn measure_next_file(scenario: &mut Scenario<CodeReviewSurface>, rows: &mut Vec<Row>) {
    // Move between ordinary modified files (200 lines, 50 rewritten in 10 hunks) with the
    // next-file key, one frame per key. Files sort by path and the ordinary files sort last, so
    // the renamed, deleted, added, and special files stay out of these samples.
    let first_ordinary = GitFixture::modified_path(0);
    let start = scenario
        .surface()
        .app()
        .review
        .files
        .iter()
        .position(|file| file_path(file) == Some(first_ordinary.as_str()))
        .expect("first ordinary modified file in review");
    assert!(scenario.surface_mut().app_mut().select_file(start));
    let _ = timed_render(scenario);
    let label = format!("next file ({WIDTH}x{HEIGHT})");
    let moves = sample_frames(scenario, &label, |scenario| {
        scenario.key("n");
    });
    let end = scenario.surface().app().selected_file;
    assert_eq!(end, start + moves.render.len());
    assert!(
        scenario.surface().app().review.files[start..=end]
            .iter()
            .all(|file| file_path(file).is_some_and(|path| path.starts_with("src/mod_"))),
        "next-file samples cover only ordinary modified files"
    );
    push(rows, frame_stats(&format!("{label} input"), moves.input));
    push(rows, frame_stats(&format!("{label} render"), moves.render));
}

#[test]
#[ignore = "phase 0 benchmark; run with --ignored --nocapture"]
fn v1_baseline_benchmark() {
    let mut rows = Vec::new();
    let started = Instant::now();
    let fixture = GitFixture::generate(FixtureSpec::benchmark());
    push(
        &mut rows,
        [row("fixture generation", ms(started.elapsed()))],
    );

    let branch = measure_materialize("branch compare", &fixture, fixture_branch_compare());
    push(&mut rows, branch.rows);
    let worktree = measure_materialize(
        "working tree + index",
        &fixture,
        ReviewTarget::WorkingTreeAndIndex,
    );
    push(&mut rows, worktree.rows);
    let (worktree_app, worktree_rows) = build_app("working tree + index", worktree.response);
    push(&mut rows, worktree_rows);
    drop(worktree_app);
    let (app, app_rows) = build_app("branch compare", branch.response);
    push(&mut rows, app_rows);

    let large = app
        .review
        .files
        .iter()
        .position(|file| file.new_path.as_deref() == Some(LARGE_FILE))
        .expect("large file in review");
    let large_rows: usize = app.review.files[large]
        .hunks
        .iter()
        .map(|hunk| hunk.lines.len())
        .sum();
    push(
        &mut rows,
        [row("large file diff lines", large_rows.to_string())],
    );
    let surface =
        CodeReviewSurface::from_app(fixture.root().to_path_buf(), fixture_branch_compare(), app);
    let mut scenario = Scenario::new(surface, WIDTH, HEIGHT);
    push(
        &mut rows,
        [row("first frame (file 0)", ms(timed_render(&mut scenario)))],
    );

    measure_next_file(&mut scenario, &mut rows);

    // Scroll the diff of the large file with the mouse wheel, one frame per wheel event.
    assert!(scenario.surface_mut().app_mut().select_file(large));
    push(
        &mut rows,
        [row(
            "first frame (large file)",
            ms(timed_render(&mut scenario)),
        )],
    );
    let diff = scenario.surface().app().diff_area().expect("diff area");
    let (x, y) = (diff.x + diff.width / 2, diff.y + diff.height / 2);
    let label = format!("wheel scroll large file ({WIDTH}x{HEIGHT})");
    let scroll = sample_frames(&mut scenario, &label, |scenario| {
        scenario.mouse(MouseEventKind::ScrollDown, x, y);
    });
    assert!(scenario.surface().app().diff_scroll > 0, "wheel scrolled");
    push(
        &mut rows,
        frame_stats(&format!("{label} input"), scroll.input),
    );
    push(
        &mut rows,
        frame_stats(&format!("{label} render"), scroll.render),
    );

    let profile = if cfg!(debug_assertions) {
        "debug (test profile)"
    } else {
        "release"
    };
    let width = rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    println!("\nV1 baseline ({profile}, fixture {:?})", fixture.spec());
    println!("{:-<1$}", "", width + 40);
    for row in &rows {
        println!("{:<width$}  {}", row.label, row.value);
    }
}
