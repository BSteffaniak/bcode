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

/// Translate one neutral runtime statistics snapshot into Bcode's secret-safe metrics namespace.
#[cfg_attr(not(test), allow(dead_code))]
pub fn record_stats(telemetry: &mut TuiTelemetry, stats: &bmux_tui_runtime::RuntimeStats) {
    telemetry.set_gauge(
        "tui.runtime.reliable_depth",
        i64::try_from(stats.reliable_depth).unwrap_or(i64::MAX),
    );
    telemetry.set_gauge(
        "tui.runtime.reliable_high_water",
        i64::try_from(stats.reliable_high_water).unwrap_or(i64::MAX),
    );
    telemetry.set_gauge(
        "tui.runtime.terminal_depth",
        i64::try_from(stats.terminal_depth).unwrap_or(i64::MAX),
    );
    telemetry.set_gauge(
        "tui.runtime.latest_depth",
        i64::try_from(stats.latest_depth).unwrap_or(i64::MAX),
    );
    telemetry.add_counter("tui.runtime.redraw_coalesced_total", stats.redraw_coalesced);
    telemetry.add_counter(
        "tui.runtime.scheduler_budget_exhausted_total",
        stats.scheduler_budget_exhausted,
    );
    telemetry.add_counter(
        "tui.runtime.stale_command_completions_total",
        stats.stale_command_completions,
    );
    telemetry.record_histogram(
        "tui.runtime.presentation_delay_us",
        stats.presentation_delay_us,
    );
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
