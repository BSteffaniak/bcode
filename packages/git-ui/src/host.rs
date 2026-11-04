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
}

/// Host with no extra capabilities.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandaloneHost;

impl GitUiHost for StandaloneHost {}
