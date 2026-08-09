//! Bcode adapters for the domain-neutral BMUX TUI runtime contracts.

use bcode_config::TuiConfig;

use super::telemetry::TuiTelemetry;

/// Build generic runtime scheduling configuration from Bcode-owned TUI settings.
#[must_use]
pub fn config(tui: &TuiConfig) -> bmux_tui_runtime::RuntimeConfig {
    bmux_tui_runtime::RuntimeConfig {
        frame_interval: tui.render.frame_interval(),
        ..bmux_tui_runtime::RuntimeConfig::default()
    }
}

/// Stateful translation of cumulative neutral runtime statistics into Bcode metrics.
#[derive(Default)]
pub struct RuntimeStatsRecorder {
    previous: bmux_tui_runtime::RuntimeStats,
}

impl RuntimeStatsRecorder {
    /// Record one runtime snapshot without double-counting cumulative counters.
    pub fn record(&mut self, telemetry: &mut TuiTelemetry, stats: &bmux_tui_runtime::RuntimeStats) {
        record_queue_gauges(telemetry, stats);
        record_admission_deltas(telemetry, stats, &self.previous);
        record_lifecycle_deltas(telemetry, stats, &self.previous);
        record_presentation_deltas(telemetry, stats, &self.previous);
        self.previous = *stats;
    }
}

fn record_queue_gauges(telemetry: &mut TuiTelemetry, stats: &bmux_tui_runtime::RuntimeStats) {
    for (name, value) in [
        ("tui.runtime.reliable_depth", stats.reliable_depth),
        ("tui.runtime.reliable_high_water", stats.reliable_high_water),
        ("tui.runtime.terminal_depth", stats.terminal_depth),
        ("tui.runtime.terminal_high_water", stats.terminal_high_water),
        ("tui.runtime.latest_depth", stats.latest_depth),
        ("tui.runtime.latest_high_water", stats.latest_high_water),
        ("tui.runtime.subscription_depth", stats.subscription_depth),
        (
            "tui.runtime.subscription_high_water",
            stats.subscription_high_water,
        ),
    ] {
        telemetry.set_gauge(name, i64::try_from(value).unwrap_or(i64::MAX));
    }
}

fn record_admission_deltas(
    telemetry: &mut TuiTelemetry,
    stats: &bmux_tui_runtime::RuntimeStats,
    previous: &bmux_tui_runtime::RuntimeStats,
) {
    for (name, current, prior) in [
        (
            "tui.runtime.reliable_rejected_total",
            stats.reliable_rejected,
            previous.reliable_rejected,
        ),
        (
            "tui.runtime.terminal_rejected_total",
            stats.terminal_rejected,
            previous.terminal_rejected,
        ),
        (
            "tui.runtime.latest_rejected_total",
            stats.latest_rejected,
            previous.latest_rejected,
        ),
        (
            "tui.runtime.latest_replaced_total",
            stats.latest_replaced,
            previous.latest_replaced,
        ),
        (
            "tui.runtime.reliable_processed_total",
            stats.reliable_processed,
            previous.reliable_processed,
        ),
        (
            "tui.runtime.terminal_processed_total",
            stats.terminal_processed,
            previous.terminal_processed,
        ),
        (
            "tui.runtime.latest_processed_total",
            stats.latest_processed,
            previous.latest_processed,
        ),
        (
            "tui.runtime.subscription_rejected_total",
            stats.subscription_rejected,
            previous.subscription_rejected,
        ),
    ] {
        add_delta(telemetry, name, current, prior);
    }
}

fn record_lifecycle_deltas(
    telemetry: &mut TuiTelemetry,
    stats: &bmux_tui_runtime::RuntimeStats,
    previous: &bmux_tui_runtime::RuntimeStats,
) {
    for (name, current, prior) in [
        (
            "tui.runtime.subscriptions_started_total",
            stats.subscriptions_started,
            previous.subscriptions_started,
        ),
        (
            "tui.runtime.subscriptions_cancelled_total",
            stats.subscriptions_cancelled,
            previous.subscriptions_cancelled,
        ),
        (
            "tui.runtime.subscriptions_completed_total",
            stats.subscriptions_completed,
            previous.subscriptions_completed,
        ),
        (
            "tui.runtime.timers_delivered_total",
            stats.timers_delivered,
            previous.timers_delivered,
        ),
        (
            "tui.runtime.commands_started_total",
            stats.commands_started,
            previous.commands_started,
        ),
        (
            "tui.runtime.commands_queued_total",
            stats.commands_queued,
            previous.commands_queued,
        ),
        (
            "tui.runtime.commands_rejected_total",
            stats.commands_rejected,
            previous.commands_rejected,
        ),
        (
            "tui.runtime.commands_cancelled_total",
            stats.commands_cancelled,
            previous.commands_cancelled,
        ),
        (
            "tui.runtime.stale_command_completions_total",
            stats.stale_command_completions,
            previous.stale_command_completions,
        ),
    ] {
        add_delta(telemetry, name, current, prior);
    }
}

fn record_presentation_deltas(
    telemetry: &mut TuiTelemetry,
    stats: &bmux_tui_runtime::RuntimeStats,
    previous: &bmux_tui_runtime::RuntimeStats,
) {
    for (name, current, prior) in [
        (
            "tui.runtime.redraw_requests_total",
            stats.redraw_requests,
            previous.redraw_requests,
        ),
        (
            "tui.runtime.redraw_coalesced_total",
            stats.redraw_coalesced,
            previous.redraw_coalesced,
        ),
        (
            "tui.runtime.frames_presented_total",
            stats.frames_presented,
            previous.frames_presented,
        ),
        (
            "tui.runtime.full_repaints_total",
            stats.full_repaints,
            previous.full_repaints,
        ),
        (
            "tui.runtime.presented_changed_cells_total",
            stats.presented_changed_cells,
            previous.presented_changed_cells,
        ),
        (
            "tui.runtime.updates_completed_total",
            stats.updates_completed,
            previous.updates_completed,
        ),
        (
            "tui.runtime.scheduler_budget_exhausted_total",
            stats.scheduler_budget_exhausted,
            previous.scheduler_budget_exhausted,
        ),
    ] {
        add_delta(telemetry, name, current, prior);
    }
    for (name, current, prior) in [
        (
            "tui.runtime.presentation_delay_us",
            stats.presentation_delay_us,
            previous.presentation_delay_us,
        ),
        (
            "tui.runtime.presentation_time_us",
            stats.presentation_time_us,
            previous.presentation_time_us,
        ),
        (
            "tui.runtime.update_time_us",
            stats.update_time_us,
            previous.update_time_us,
        ),
    ] {
        let value = current.saturating_sub(prior);
        if value > 0 {
            telemetry.record_histogram(name, value);
        }
    }
}

fn add_delta(telemetry: &mut TuiTelemetry, name: &str, current: u64, previous: u64) {
    let delta = current.saturating_sub(previous);
    if delta > 0 {
        telemetry.add_counter(name, delta);
    }
}

#[cfg(test)]
mod tests {
    use super::config;
    use bcode_config::{TuiConfig, TuiRenderConfig};
    use std::time::Duration;

    #[test]
    fn config_maps_frame_interval_without_product_types_crossing_boundary() {
        let enabled = config(&TuiConfig {
            render: TuiRenderConfig { max_fps: 20 },
            ..TuiConfig::default()
        });
        assert_eq!(enabled.frame_interval, Some(Duration::from_millis(50)));

        let unlimited = config(&TuiConfig {
            render: TuiRenderConfig { max_fps: 0 },
            ..TuiConfig::default()
        });
        assert_eq!(unlimited.frame_interval, None);
    }
}
