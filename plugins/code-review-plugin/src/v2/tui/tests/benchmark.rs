//! V2 review surface benchmark on the phase 0 fixture (ignored by default).
//!
//! Run with:
//!
//! ```text
//! cargo test -p bcode_code_review_plugin v2_screen_benchmark -- --ignored --nocapture
//! ```
//!
//! Frames are rendered at 200x60 through the scenario harness against the real V2 service
//! in-process, with the default panels (file tree on the left). Only `render` is timed: service
//! calls run between frames, as on the host.

use super::surface;
use crate::v2::document::ReviewItem;
use crate::v2::testing::fixture::{FixtureSpec, GitFixture, LARGE_FILE};
use crate::v2::testing::fixture_branch_compare;
use crate::v2::testing::harness::Scenario;
use crate::v2::tui::ReviewSurfaceV2;
use crate::v2::tui::config::ReviewViewConfig;
use crate::v2::tui::files::FileTree;
use crate::v2::tui::theme::{IconStyle, ReviewTheme};
use bmux_tui::event::MouseEventKind;
use std::time::{Duration, Instant};

const WIDTH: u16 = 200;
const HEIGHT: u16 = 60;

fn ms(duration: Duration) -> String {
    format!("{:.3} ms", duration.as_secs_f64() * 1_000.0)
}

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let total: Duration = samples.iter().sum();
    let count = u32::try_from(samples.len()).expect("sample count").max(1);
    let p95 = samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)];
    println!(
        "{label}: {} frames, avg {}, p95 {}, max {}",
        samples.len(),
        ms(total / count),
        ms(p95),
        ms(*samples.last().expect("samples"))
    );
}

/// Settle async work (untimed), then time one render.
fn timed_frame(scenario: &mut Scenario<ReviewSurfaceV2>) -> Duration {
    scenario.settle();
    let started = Instant::now();
    scenario.render();
    started.elapsed()
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
#[allow(clippy::too_many_lines, reason = "one sequential benchmark script")]
fn v2_screen_benchmark() {
    let started = Instant::now();
    let fixture = GitFixture::generate(FixtureSpec::benchmark());
    println!("fixture generated in {}", ms(started.elapsed()));
    let mut scenario = Scenario::new(
        surface(
            &fixture,
            fixture_branch_compare(),
            ReviewViewConfig::default(),
        ),
        WIDTH,
        HEIGHT,
    );

    // Index: open until the document exists.
    let started = Instant::now();
    scenario.render();
    // Step async work only, so the measurement below starts as soon as the document exists
    // (settling would also load and highlight content before returning).
    while scenario.surface().document().is_none() {
        scenario.step_async();
    }
    println!("open (resolve + whole index): {}", ms(started.elapsed()));

    // Time to the first frame with the first file's content, after the index returned (the
    // document was built): hunk loads plus every frame until the content is painted. Async
    // work runs as it would on the host; syntax highlighting (blocking-pool work) is left
    // running, since rows paint unhighlighted until it is ready. The harness runs blocking
    // jobs serially on the test thread, so settling them here would add every visible file's
    // highlighting to the latency.
    let mut renders = Vec::new();
    loop {
        let render = Instant::now();
        let screen = scenario.render();
        renders.push(render.elapsed());
        if screen.contains("@@") {
            break;
        }
        assert!(renders.len() < 1_000, "content never painted");
        scenario.step_async();
    }
    let opened = scenario.surface().opened_at().expect("opened");
    let content = opened.elapsed();
    scenario.settle();
    scenario.render();
    println!(
        "first frame with content after the index: {}; highlighted after {} ({} renders: {})",
        ms(content),
        ms(opened.elapsed()),
        renders.len(),
        renders
            .iter()
            .map(|render| ms(*render))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // The file tree alone, rebuilt from the opened document.
    let doc = scenario.surface().document().expect("document");
    let theme = ReviewTheme::derive(None, IconStyle::Unicode);
    let build = Instant::now();
    let tree = FileTree::new(doc, &theme);
    println!(
        "file tree: {} rows for {} files built in {}",
        tree.items().len(),
        tree.file_count(),
        ms(build.elapsed())
    );

    // Steady scrolling through the 5k-file review: wheel steps, with a page step every tenth
    // frame and a jump of 50 files every 100 frames to sample the whole review.
    let mut samples = Vec::new();
    let budget = Instant::now();
    for frame in 0..3_000 {
        if budget.elapsed() > Duration::from_secs(90) {
            break;
        }
        if frame % 100 == 99 {
            scenario.key("5");
            scenario.key("0");
            scenario.key("n");
        } else if frame % 10 == 9 {
            scenario.key("pagedown");
        } else {
            let list = scenario.surface().list_area().expect("list");
            scenario.mouse(MouseEventKind::ScrollDown, list.x + 10, list.y + 10);
        }
        samples.push(timed_frame(&mut scenario));
    }
    report("scroll 5k-file review", samples);
    let doc = scenario.surface().document().expect("document");
    println!(
        "resident: {:.1} MiB, {} files; rows built {}",
        f64::from(u32::try_from(doc.resident_bytes() / 1024).unwrap_or(u32::MAX)) / 1024.0,
        doc.resident_files().count(),
        scenario.surface().rows_built()
    );

    // The 40k-line file: jump to it, then scroll.
    let doc = scenario.surface().document().expect("document");
    let header = (0..doc.len())
        .find(|index| {
            matches!(doc.item(*index), Some(ReviewItem::FileHeader(header)) if header.file.path == LARGE_FILE)
        })
        .expect("large file");
    scenario.key("g");
    scenario.settle();
    scenario.render();
    for digit in header.to_string().chars() {
        scenario.key(&digit.to_string());
    }
    scenario.key("j");
    let jump = timed_frame(&mut scenario);
    let started = Instant::now();
    let mut first = Vec::new();
    for _ in 0..50 {
        let duration = timed_frame(&mut scenario);
        first.push(duration);
        if scenario.surface().outstanding_requests() == 0 && scenario.host().pending_tasks() == 0 {
            break;
        }
    }
    println!(
        "jump to the 40k-line file: {}; loaded and shown after {} ({} frames)",
        ms(jump),
        ms(started.elapsed()),
        first.len()
    );
    let mut samples = Vec::new();
    let budget = Instant::now();
    for frame in 0..2_000 {
        if budget.elapsed() > Duration::from_secs(60) {
            break;
        }
        if frame % 10 == 9 {
            scenario.key("pagedown");
        } else {
            let list = scenario.surface().list_area().expect("list");
            scenario.mouse(MouseEventKind::ScrollDown, list.x + 10, list.y + 10);
        }
        samples.push(timed_frame(&mut scenario));
    }
    report("scroll 40k-line file", samples);
}
