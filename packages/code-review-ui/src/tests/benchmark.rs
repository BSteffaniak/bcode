//! Review screen benchmark on the benchmark fixture (ignored by default).
//!
//! Run with:
//!
//! ```text
//! cargo test --release -p bcode_code_review_ui screen_benchmark -- --ignored --nocapture
//! ```
//!
//! Frames are rendered at 200x60 through the scenario harness against the real data layer
//! in-process, with the default panels (file tree on the left). Only `render` is timed: service
//! calls run between frames, as on the host.
//!
//! `screen_benchmark_sizes` repeats the scroll phases at larger terminals in both diff
//! layouts and presents every frame through a BMUX terminal, reporting paint, diff-encode, and
//! input-handling time and the bytes each frame writes.

use super::chrome::{host_theme, rgb};
use super::surface;
use crate::ReviewSurface;
use crate::config::{ConfiguredLayout, ReviewViewConfig};
use crate::files::FileTree;
use crate::testing::fixture::{FixtureSpec, GitFixture, LARGE_FILE};
use crate::testing::fixture_branch_compare;
use crate::testing::harness::Scenario;
use crate::theme::{IconStyle, ReviewTheme};
use bcode_code_review::document::ReviewItem;
use bmux_tui::event::MouseEventKind;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
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
fn timed_frame(scenario: &mut Scenario<ReviewSurface>) -> Duration {
    scenario.settle();
    let started = Instant::now();
    scenario.render();
    started.elapsed()
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
#[allow(clippy::too_many_lines, reason = "one sequential benchmark script")]
fn screen_benchmark() {
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

/// Per-frame costs of one benchmark phase, presented through a BMUX terminal as the host does.
#[derive(Default)]
struct PhaseCosts {
    /// Input handling (every event routed before the frame).
    event: Vec<Duration>,
    /// Painting a prepared presentation with full damage (surface render, layout, hits).
    paint: Vec<Duration>,
    /// Diff-encoding against the retained frame and writing it (into memory).
    encode: Vec<Duration>,
    /// Bytes written to the terminal for the frame.
    bytes: Vec<usize>,
    /// Cells the diff changed.
    cells: Vec<usize>,
}

fn stats(samples: &[Duration]) -> (Duration, Duration, Duration) {
    if samples.is_empty() {
        return (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let count = u32::try_from(sorted.len()).expect("sample count").max(1);
    let p95 = sorted[(sorted.len() * 95).div_ceil(100).saturating_sub(1)];
    (
        sorted.iter().sum::<Duration>() / count,
        p95,
        *sorted.last().expect("samples"),
    )
}

impl PhaseCosts {
    fn report(&self, case: &str, phase: &str) {
        let (paint, paint_p95, paint_max) = stats(&self.paint);
        let (encode, encode_p95, _) = stats(&self.encode);
        let (event, event_p95, _) = stats(&self.event);
        let frames = self.bytes.len();
        let bytes = self.bytes.iter().sum::<usize>() / frames.max(1);
        let cells = self.cells.iter().sum::<usize>() / frames.max(1);
        println!(
            "{case} | {phase}: {frames} frames | paint avg {} p95 {} max {} | encode avg {} p95 \
             {} | event avg {} p95 {} | {bytes} B/frame avg, max {} B | {cells} cells/frame",
            ms(paint),
            ms(paint_p95),
            ms(paint_max),
            ms(encode),
            ms(encode_p95),
            ms(event),
            ms(event_p95),
            self.bytes.iter().max().copied().unwrap_or(0),
        );
    }
}

/// Settle async work (untimed), then paint and present one frame, recording its costs.
fn present(
    scenario: &mut Scenario<ReviewSurface>,
    terminal: &mut Terminal<Vec<u8>>,
    costs: &mut PhaseCosts,
) {
    scenario.settle();
    let started = Instant::now();
    let prepared = scenario.prepare(terminal);
    costs.paint.push(started.elapsed());
    terminal.writer_mut().clear();
    let started = Instant::now();
    let drawn = terminal
        .draw_prepared_with_output_hooks(prepared, |_| Ok(false), |_, _, _| Ok(()))
        .expect("draw");
    costs.encode.push(started.elapsed());
    costs.bytes.push(terminal.writer().len());
    costs.cells.push(drawn.changed_cells);
}

/// Time the input events `send` routes, then present a frame.
fn step(
    scenario: &mut Scenario<ReviewSurface>,
    terminal: &mut Terminal<Vec<u8>>,
    costs: &mut PhaseCosts,
    send: impl FnOnce(&mut Scenario<ReviewSurface>),
) {
    let started = Instant::now();
    send(scenario);
    costs.event.push(started.elapsed());
    present(scenario, terminal, costs);
}

fn wheel_down(scenario: &mut Scenario<ReviewSurface>) {
    let list = scenario.surface().list_area().expect("list");
    scenario.mouse(MouseEventKind::ScrollDown, list.x + 10, list.y + 10);
}

/// One terminal size and diff layout of the large-terminal benchmark.
#[allow(clippy::too_many_lines, reason = "one sequential benchmark script")]
fn sized_case(fixture: &GitFixture, width: u16, height: u16, layout: ConfiguredLayout) {
    const FRAMES: usize = 400;
    const PHASE_BUDGET: Duration = Duration::from_secs(25);
    let case = format!("{width}x{height} {layout:?}");
    let config = ReviewViewConfig {
        diff_layout: Some(layout),
        ..ReviewViewConfig::default()
    };
    let mut scenario = Scenario::new(
        surface(fixture, fixture_branch_compare(), config),
        width,
        height,
    );
    scenario.set_theme(Some(&host_theme(rgb(190, 120, 250))));
    let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, width, height));

    // First frame with content after the index (paint only, as in `screen_benchmark`).
    scenario.render();
    while scenario.surface().document().is_none() {
        scenario.step_async();
    }
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
    let content = scenario.surface().opened_at().expect("opened").elapsed();
    let (paint, _, paint_max) = stats(&renders);
    println!(
        "{case} | first content frame after the index: {} ({} renders, avg {} max {})",
        ms(content),
        renders.len(),
        ms(paint),
        ms(paint_max)
    );

    // The first presented frame is a complete repaint.
    let mut full = PhaseCosts::default();
    present(&mut scenario, &mut terminal, &mut full);
    full.report(&case, "first presentation (full repaint)");

    // The 5k-file review, scrolled as in `screen_benchmark`.
    let mut costs = PhaseCosts::default();
    let budget = Instant::now();
    for frame in 0..FRAMES {
        if budget.elapsed() > PHASE_BUDGET {
            break;
        }
        step(&mut scenario, &mut terminal, &mut costs, |scenario| {
            if frame % 100 == 99 {
                scenario.keys(&["5", "0", "n"]);
            } else if frame % 10 == 9 {
                scenario.key("pagedown");
            } else {
                wheel_down(scenario);
            }
        });
    }
    costs.report(&case, "scroll 5k-file review");

    // The same frame again after a terminal reset: what a forced full redraw costs.
    terminal.reset();
    let mut full = PhaseCosts::default();
    present(&mut scenario, &mut terminal, &mut full);
    full.report(&case, "full redraw after reset");

    // The 40k-line file: jump to it, then wheel and page through it.
    let doc = scenario.surface().document().expect("document");
    let header = (0..doc.len())
        .find(|index| {
            matches!(doc.item(*index), Some(ReviewItem::FileHeader(header)) if header.file.path == LARGE_FILE)
        })
        .expect("large file");
    scenario.key("g");
    let mut ignored = PhaseCosts::default();
    present(&mut scenario, &mut terminal, &mut ignored);
    for digit in header.to_string().chars() {
        scenario.key(&digit.to_string());
    }
    scenario.key("j");
    for _ in 0..50 {
        present(&mut scenario, &mut terminal, &mut ignored);
        if scenario.surface().outstanding_requests() == 0 && scenario.host().pending_tasks() == 0 {
            break;
        }
    }
    let mut costs = PhaseCosts::default();
    let budget = Instant::now();
    for frame in 0..FRAMES {
        if budget.elapsed() > PHASE_BUDGET {
            break;
        }
        step(&mut scenario, &mut terminal, &mut costs, |scenario| {
            if frame % 10 == 9 {
                scenario.key("pagedown");
            } else {
                wheel_down(scenario);
            }
        });
    }
    costs.report(&case, "scroll 40k-line file");

    // Held keys in the 40k-line file: one `j` per frame, then one `pagedown` per frame.
    for (phase, key) in [("held j", "j"), ("held pagedown", "pagedown")] {
        let mut costs = PhaseCosts::default();
        let budget = Instant::now();
        for _ in 0..FRAMES / 2 {
            if budget.elapsed() > PHASE_BUDGET {
                break;
            }
            step(&mut scenario, &mut terminal, &mut costs, |scenario| {
                scenario.key(key);
            });
        }
        costs.report(&case, phase);
    }

    // Repeats queued while a frame was being drawn: ten `j` handled, then one frame.
    let mut costs = PhaseCosts::default();
    for _ in 0..50 {
        step(&mut scenario, &mut terminal, &mut costs, |scenario| {
            scenario.keys(&["j"; 10]);
        });
    }
    costs.report(&case, "10 queued j per frame");
}

/// Large-terminal matrix: unified and side-by-side layouts at 200x60, 300x90, and 400x120,
/// presented through a BMUX terminal (paint, diff-encode, and bytes per frame).
///
/// Run with:
///
/// ```text
/// cargo test --release -p bcode_code_review_ui screen_benchmark_sizes -- --ignored --nocapture
/// ```
///
/// Set `BCODE_BENCH_SIZES` (for example `400x120,200x60`) to choose sizes.
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn screen_benchmark_sizes() {
    let started = Instant::now();
    let fixture = GitFixture::generate(FixtureSpec::benchmark());
    println!("fixture generated in {}", ms(started.elapsed()));
    let sizes = std::env::var("BCODE_BENCH_SIZES").map_or_else(
        |_| vec![(200, 60), (300, 90), (400, 120)],
        |sizes| {
            sizes
                .split(',')
                .map(|size| {
                    let (width, height) = size.split_once('x').expect("WIDTHxHEIGHT");
                    (
                        width.trim().parse().expect("width"),
                        height.trim().parse().expect("height"),
                    )
                })
                .collect()
        },
    );
    for (width, height) in sizes {
        for layout in [ConfiguredLayout::Unified, ConfiguredLayout::SideBySide] {
            sized_case(&fixture, width, height, layout);
        }
    }
}
