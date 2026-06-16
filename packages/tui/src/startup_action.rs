//! Startup actions that can run after the main TUI context is initialized.

/// Optional action to run as soon as the main TUI starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartupTuiAction {
    /// Start normally.
    #[default]
    None,
    /// Open the plugin-owned Ralph home UI.
    OpenRalphHome,
}
