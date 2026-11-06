//! Extension seam for hosts that embed the git UI.
//!
//! The standalone binary uses [`StandaloneHost`]. A richer host (for example bcode with agent
//! access) can contribute menu entries without the UI depending on that host.

use crate::popup::MenuItem;

/// What a host contribution relates to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostContext {
    /// Staged changes about to be committed.
    StagedChanges,
    /// A selected commit.
    Commit(String),
    /// A selected file path.
    File(String),
}

/// Host-provided capabilities. Every method has a neutral default.
pub trait GitUiHost: Send + Sync {
    /// Host name shown in the UI chrome.
    fn name(&self) -> String {
        "bcode git".to_owned()
    }

    /// Extra menu entries for a context, appended to the relevant options menus.
    fn menu_items(&self, _context: &HostContext) -> Vec<MenuItem> {
        Vec::new()
    }

    /// Open a web page (such as a pull request) in the user's browser, without suspending the
    /// UI. The default runs the platform opener (`open`, `xdg-open`, `start`).
    ///
    /// # Errors
    /// Returns a description when the opener cannot be started.
    fn open_url(&self, url: &str) -> Result<(), String> {
        let mut command = if cfg!(target_os = "macos") {
            std::process::Command::new("open")
        } else if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "start", ""]);
            command
        } else {
            std::process::Command::new("xdg-open")
        };
        command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// Host with no extra capabilities.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandaloneHost;

impl GitUiHost for StandaloneHost {}
