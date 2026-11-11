//! Smoke tests proving the scenario harness drives the V1 review surface.

use super::fixture::{FixtureSpec, GitFixture, LARGE_FILE};
use super::harness::Scenario;
use super::{fixture_branch_compare, materialize_v1};
use crate::code_review_tui::{
    CodeReviewSurface, ReviewApp, review_summary_from_materialization,
    tests::{sample_app, test_plugin_theme},
};
use bcode_code_review_models::ReviewTarget;
use bmux_tui::event::MouseEventKind;
use std::path::PathBuf;

fn fixture_surface(fixture: &GitFixture) -> CodeReviewSurface {
    let target = fixture_branch_compare();
    let summary =
        review_summary_from_materialization(materialize_v1(fixture.root(), target.clone()));
    CodeReviewSurface::from_app(
        fixture.root().to_path_buf(),
        target,
        ReviewApp::new(summary),
    )
}

#[test]
fn sample_review_next_file_key_changes_the_rendered_diff() {
    let surface = CodeReviewSurface::from_app(
        PathBuf::from("/repo"),
        ReviewTarget::WorkingTreeUnstaged,
        sample_app(),
    );
    let mut scenario = Scenario::new(surface, 120, 30);
    let first = scenario.frame();
    assert!(first.contains("a.rs"), "{}", first.text());
    assert_eq!(scenario.surface().app().selected_file, 0);

    scenario.key("n");
    let second = scenario.frame();
    assert_eq!(scenario.surface().app().selected_file, 1);
    assert_ne!(first.text(), second.text());
}

#[test]
fn presentation_theme_reaches_the_rendered_cell_styles() {
    let surface = CodeReviewSurface::from_app(
        PathBuf::from("/repo"),
        ReviewTarget::WorkingTreeUnstaged,
        sample_app(),
    );
    let mut scenario = Scenario::new(surface, 120, 30);
    scenario.set_theme(Some(&test_plugin_theme(10)));
    let dim = scenario.frame();
    scenario.set_theme(Some(&test_plugin_theme(250)));
    let bright = scenario.frame();
    assert_eq!(dim.text(), bright.text(), "theme changes styling only");
    let restyled = (0..dim.height())
        .flat_map(|y| (0..dim.width()).map(move |x| (x, y)))
        .any(|(x, y)| dim.style_at(x, y) != bright.style_at(x, y));
    assert!(restyled, "syntax palette change is visible in cell styles");
}

#[test]
fn fixture_review_handles_several_events_between_frames() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut scenario = Scenario::new(fixture_surface(&fixture), 160, 40);
    let first = scenario.frame();
    let files = scenario.surface().app().review.files.len();
    assert!(files >= 8, "fixture produced {files} files");

    scenario.keys(&["n", "n", "n"]);
    assert_eq!(scenario.surface().app().selected_file, 3);
    scenario.key("p");
    let after = scenario.frame();
    assert_eq!(scenario.surface().app().selected_file, 2);
    let selected = scenario
        .surface()
        .app()
        .selected_file_path()
        .expect("selected path");
    let name = selected.rsplit('/').next().unwrap_or(&selected);
    assert!(after.contains(name), "{name} not shown:\n{}", after.text());
    assert_ne!(first.text(), after.text());
}

#[test]
fn fixture_review_wheel_scroll_and_resize_change_frames() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut scenario = Scenario::new(fixture_surface(&fixture), 160, 40);
    let large = scenario
        .surface()
        .app()
        .review
        .files
        .iter()
        .position(|file| file.new_path.as_deref() == Some(LARGE_FILE))
        .expect("large file in review");
    assert!(scenario.surface_mut().app_mut().select_file(large));
    let before = scenario.frame();
    let diff = scenario
        .surface()
        .app()
        .diff_area()
        .expect("diff area painted");
    let (x, y) = (diff.x + diff.width / 2, diff.y + diff.height / 2);

    for _ in 0..4 {
        scenario.mouse(MouseEventKind::ScrollDown, x, y);
    }
    let scrolled = scenario.frame();
    assert!(scenario.surface().app().diff_scroll > 0);
    assert_ne!(before.text(), scrolled.text());

    scenario.resize(80, 20);
    let narrow = scenario.frame();
    assert_eq!((narrow.width(), narrow.height()), (80, 20));
    let narrow_diff = scenario.surface().app().diff_area().expect("diff area");
    assert!(narrow_diff.right() <= 80 && narrow_diff.bottom() <= 20);
}
