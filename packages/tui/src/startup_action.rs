//! Startup actions that can run after the main TUI context is initialized.

use std::path::PathBuf;

/// Optional action to run as soon as the main TUI starts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartupTuiAction {
    /// Start normally.
    #[default]
    None,
    /// Open the plugin-owned Ralph home UI for a resolved repository path.
    OpenRalphHome {
        /// Absolute repository path used as Ralph surface context.
        repo_path: PathBuf,
    },
}
