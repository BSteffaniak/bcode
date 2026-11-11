//! V2 review surface tests, driven through the phase 0 scenario harness against the real V2
//! service (in-process) over generated fixture repositories.

mod app;
mod benchmark;
mod chrome;
mod interaction;
mod parity;
mod publish;
mod rereview;
mod screen;
mod wizard;

use super::ReviewSurfaceV2;
use super::config::ReviewViewConfig;
use crate::v2::testing::fixture::GitFixture;
use crate::v2::testing::harness::{Scenario, Screen};
use crate::v2::testing::local_backend::LocalBackend;
use bcode_code_review_models::{ReviewTarget, ReviewWorkspace};
use std::sync::Arc;

/// A surface reviewing `target` in `fixture`.
fn surface(
    fixture: &GitFixture,
    target: ReviewTarget,
    config: ReviewViewConfig,
) -> ReviewSurfaceV2 {
    let workspace = ReviewWorkspace::from_target(fixture.root().to_path_buf(), target);
    ReviewSurfaceV2::new(
        Arc::new(LocalBackend::default()),
        fixture.root().to_path_buf(),
        workspace.sources,
        workspace.title,
        config,
    )
    .with_scroll_motion(std::time::Duration::ZERO)
}

/// Render and settle until the surface issues no more work and the screen stops changing.
fn stable(scenario: &mut Scenario<ReviewSurfaceV2>) -> Screen {
    let mut previous = scenario.frame();
    for _ in 0..64 {
        let screen = scenario.frame();
        if screen.text() == previous.text()
            && scenario.surface().outstanding_requests() == 0
            && scenario.host().pending_tasks() == 0
        {
            return screen;
        }
        previous = screen;
    }
    panic!("screen did not stabilize:\n{}", previous.text());
}

/// Open `target` at `width` x `height` and wait until the first screen is stable.
fn open(
    fixture: &GitFixture,
    target: ReviewTarget,
    width: u16,
    height: u16,
) -> (Scenario<ReviewSurfaceV2>, Screen) {
    let mut scenario = Scenario::new(
        surface(fixture, target, ReviewViewConfig::default()),
        width,
        height,
    );
    let screen = stable(&mut scenario);
    (scenario, screen)
}
