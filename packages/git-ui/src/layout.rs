//! Side-window arrangement resolved from [`LayoutConfig`].

use std::collections::BTreeSet;

use crate::app::View;
use crate::config::LayoutConfig;

/// Views a user may place in side windows, with their configuration names.
const PLACEABLE: [(&str, View); 10] = [
    ("status", View::Status),
    ("files", View::Files),
    ("worktrees", View::Worktrees),
    ("submodules", View::Submodules),
    ("branches", View::Branches),
    ("remotes", View::Remotes),
    ("tags", View::Tags),
    ("commits", View::Commits),
    ("reflog", View::Reflog),
    ("stash", View::Stash),
];

/// Where the UI starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// A side view.
    View(View),
    /// The main view.
    Main,
}

/// Validated side windows and startup focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Side windows from top to bottom; each holds its tabs in order. Never empty.
    pub windows: Vec<Vec<View>>,
    /// Startup focus.
    pub start: Start,
}

impl Layout {
    /// Resolve a configuration, reporting problems and falling back to defaults for them.
    #[must_use]
    pub fn resolve(config: &LayoutConfig) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let mut seen = BTreeSet::new();
        let mut windows = Vec::new();
        for tabs in &config.windows {
            let mut window = Vec::new();
            for name in tabs {
                match view_named(name) {
                    Some(view) if seen.insert(view) => window.push(view),
                    Some(_) => diagnostics.push(format!(
                        "layout.windows: \"{name}\" appears more than once; later uses are ignored"
                    )),
                    None => diagnostics.push(format!(
                        "layout.windows: unknown view \"{name}\" (expected one of {})",
                        names()
                    )),
                }
            }
            if !window.is_empty() {
                windows.push(window);
            }
        }
        if windows.is_empty() {
            if !config.windows.is_empty() {
                diagnostics.push("layout.windows has no usable views; using the default".into());
            }
            let (layout, more) = Self::resolve(&LayoutConfig {
                windows: LayoutConfig::default().windows,
                ..config.clone()
            });
            diagnostics.extend(more);
            return (layout, diagnostics);
        }
        let start = match config.initial_window.as_str() {
            "main" => Start::Main,
            name => match view_named(name) {
                Some(view) if seen.contains(&view) => Start::View(view),
                Some(_) => {
                    diagnostics.push(format!(
                        "layout.initial_window: \"{name}\" is not in layout.windows"
                    ));
                    Start::View(windows[0][0])
                }
                None => {
                    diagnostics.push(format!(
                        "layout.initial_window: unknown view \"{name}\" (expected main or one of {})",
                        names()
                    ));
                    Start::View(windows[0][0])
                }
            },
        };
        (Self { windows, start }, diagnostics)
    }

    /// Index of the window holding a view and the view's tab index there.
    #[must_use]
    pub fn position(&self, view: View) -> Option<(usize, usize)> {
        self.windows.iter().enumerate().find_map(|(window, tabs)| {
            tabs.iter()
                .position(|tab| *tab == view)
                .map(|tab| (window, tab))
        })
    }
}

/// Configuration name of a placeable view (`"files"`, `"commits"`, …); other views use their
/// label.
#[must_use]
pub fn view_name(view: View) -> &'static str {
    PLACEABLE
        .iter()
        .find(|(_, candidate)| *candidate == view)
        .map_or_else(|| view.label(), |(name, _)| *name)
}

fn view_named(name: &str) -> Option<View> {
    PLACEABLE
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name.trim()))
        .map(|(_, view)| *view)
}

fn names() -> String {
    PLACEABLE
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(windows: &[&[&str]], initial: &str) -> LayoutConfig {
        LayoutConfig {
            windows: windows
                .iter()
                .map(|tabs| tabs.iter().map(|tab| (*tab).to_owned()).collect())
                .collect(),
            initial_window: initial.to_owned(),
            ..LayoutConfig::default()
        }
    }

    #[test]
    fn default_matches_lazygit_arrangement() {
        let (layout, diagnostics) = Layout::resolve(&LayoutConfig::default());
        assert_eq!(diagnostics, Vec::<String>::new());
        assert_eq!(layout.windows.len(), 5);
        assert_eq!(layout.position(View::Tags), Some((2, 2)));
        assert_eq!(layout.start, Start::View(View::Files));
    }

    #[test]
    fn regroups_hides_and_reports_bad_entries() {
        let (layout, diagnostics) = Layout::resolve(&config(
            &[
                &["files", "stash"],
                &["commits", "Branches", "bogus", "files"],
                &[],
            ],
            "reflog",
        ));
        assert_eq!(
            layout.windows,
            [
                vec![View::Files, View::Stash],
                vec![View::Commits, View::Branches]
            ]
        );
        assert_eq!(layout.position(View::Status), None);
        // Unknown view, duplicate, and a hidden initial view.
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        assert_eq!(layout.start, Start::View(View::Files));
    }

    #[test]
    fn empty_arrangement_falls_back_to_default() {
        let (layout, diagnostics) = Layout::resolve(&config(&[&["nope"]], "main"));
        assert_eq!(layout.windows.len(), 5);
        assert_eq!(layout.start, Start::Main);
        assert_eq!(diagnostics.len(), 2);
    }
}
