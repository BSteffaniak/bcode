//! Branch name colors: what a color says about a branch, the same wherever its name appears.
//!
//! A branch's color comes from an ordered list of rules, first match wins: the configured
//! rules (`[branches] colors`), then one built-in rule per [`BranchState`]. A rule can match
//! the name (a regular expression), the state, or both. Branches merged into the base branch
//! are dimmed on top of their color. Remote branches have no state of their own: their branch
//! part takes a matching name rule, else the remote color.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bcode_git::models::Branch;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::theme::GitTheme;

/// Where a local branch stands; each branch has exactly one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BranchState {
    /// Checked out in this worktree.
    Current,
    /// Level with its upstream.
    Synced,
    /// Has commits its upstream does not.
    Ahead,
    /// Its upstream has commits it does not.
    Behind,
    /// Both ahead and behind.
    Diverged,
    /// Its upstream was deleted.
    Gone,
    /// Has no upstream.
    Local,
}

impl BranchState {
    /// Every state, in legend order.
    pub const ALL: [Self; 7] = [
        Self::Current,
        Self::Synced,
        Self::Ahead,
        Self::Behind,
        Self::Diverged,
        Self::Gone,
        Self::Local,
    ];

    /// The state of `branch`.
    #[must_use]
    pub const fn of(branch: &Branch) -> Self {
        if branch.is_head {
            Self::Current
        } else if branch.upstream_gone {
            Self::Gone
        } else if branch.upstream.is_none() {
            Self::Local
        } else {
            match (branch.ahead > 0, branch.behind > 0) {
                (true, true) => Self::Diverged,
                (true, false) => Self::Ahead,
                (false, true) => Self::Behind,
                (false, false) => Self::Synced,
            }
        }
    }

    /// Configuration name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Synced => "synced",
            Self::Ahead => "ahead",
            Self::Behind => "behind",
            Self::Diverged => "diverged",
            Self::Gone => "gone",
            Self::Local => "local",
        }
    }

    /// What the state means, for the legend.
    #[must_use]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::Current => "checked out here",
            Self::Synced => "level with its upstream",
            Self::Ahead => "has commits to push",
            Self::Behind => "has commits to pull",
            Self::Diverged => "has commits to push and to pull",
            Self::Gone => "its upstream was deleted",
            Self::Local => "has no upstream",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// States a rule applies to: one, or any of several.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum States {
    /// One state.
    One(BranchState),
    /// Any of these states.
    Any(Vec<BranchState>),
}

impl States {
    fn list(&self) -> Vec<BranchState> {
        match self {
            Self::One(state) => vec![*state],
            Self::Any(states) => states.clone(),
        }
    }
}

/// A branch color rule (`[[branches.colors]]`). It applies when every condition it has
/// holds; a rule without conditions applies to every branch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchColorRule {
    /// Regular expression the branch name must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// State (or list of states) the branch must be in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<States>,
    /// Color: a name (`cyan`, `bright-magenta`), `#rrggbb`, or a theme palette color. When
    /// unset, the branch keeps its state's color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Bold text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
}

/// Branch display settings (`[branches]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BranchesConfig {
    /// Color rules, checked in order before the built-in color for each state.
    pub colors: Vec<BranchColorRule>,
    /// Dim branches already merged into the base branch (the remote's default branch, else
    /// `main` or `master`).
    pub dim_merged: bool,
}

impl Default for BranchesConfig {
    fn default() -> Self {
        Self {
            colors: Vec::new(),
            dim_merged: true,
        }
    }
}

/// A compiled rule.
#[derive(Debug, Clone)]
struct Rule {
    pattern: Option<Regex>,
    states: Vec<BranchState>,
    style: Style,
    description: String,
}

impl Rule {
    fn matches(&self, name: &str, state: Option<BranchState>) -> bool {
        let named = self
            .pattern
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(name));
        let stated =
            self.states.is_empty() || state.is_some_and(|state| self.states.contains(&state));
        named && stated
    }
}

/// How branch names are colored: the rules, plus what is known about each local branch.
/// Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct BranchColors {
    rules: Arc<[Rule]>,
    states: [Style; 7],
    merged: Style,
    remote: Style,
    dim_merged: bool,
    known: Arc<BTreeMap<String, (BranchState, bool)>>,
}

impl BranchColors {
    /// Compile the configured rules against `theme`, reporting rules that cannot be used
    /// (they are skipped).
    #[must_use]
    pub fn new(config: &BranchesConfig, theme: &GitTheme) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let mut rules = Vec::new();
        for (index, rule) in config.colors.iter().enumerate() {
            let at = format!("branches.colors[{index}]");
            let pattern = match rule.pattern.as_deref().map(Regex::new).transpose() {
                Ok(pattern) => pattern,
                Err(error) => {
                    diagnostics.push(format!("{at}: invalid pattern: {error}"));
                    continue;
                }
            };
            let mut style = Style::new();
            if let Some(name) = &rule.color {
                let Some(color) = parse_color(name, &theme.palette) else {
                    diagnostics.push(format!(
                        "{at}: unknown color \"{name}\" (use a color name, #rrggbb, or a theme color)"
                    ));
                    continue;
                };
                style = style.fg(color);
            }
            if rule.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            let states = rule.state.as_ref().map(States::list).unwrap_or_default();
            let mut description = Vec::new();
            if let Some(pattern) = &rule.pattern {
                description.push(format!("/{pattern}/"));
            }
            if !states.is_empty() {
                description.push(
                    states
                        .iter()
                        .map(|state| state.name())
                        .collect::<Vec<_>>()
                        .join(" or "),
                );
            }
            if description.is_empty() {
                description.push("every branch".into());
            }
            rules.push(Rule {
                pattern,
                states,
                style,
                description: description.join(", "),
            });
        }
        (
            Self {
                rules: rules.into(),
                states: theme.branch_states,
                merged: theme.branch_merged,
                remote: theme.branch_remote,
                dim_merged: config.dim_merged,
                known: Arc::default(),
            },
            diagnostics,
        )
    }

    /// The same rules with the current local branches and which are merged.
    #[must_use]
    pub fn with_branches(&self, branches: &[Branch], merged: &BTreeSet<String>) -> Self {
        Self {
            known: Arc::new(
                branches
                    .iter()
                    .map(|branch| {
                        (
                            branch.name.clone(),
                            (BranchState::of(branch), merged.contains(&branch.name)),
                        )
                    })
                    .collect(),
            ),
            ..self.clone()
        }
    }

    /// The color of a local branch called `name` in `state`.
    #[must_use]
    pub fn style(&self, name: &str, state: BranchState, merged: bool) -> Style {
        let base = self.states[state.index()];
        let style = self
            .rules
            .iter()
            .find(|rule| rule.matches(name, Some(state)))
            .map_or(base, |rule| base.patch(rule.style));
        if merged && self.dim_merged && state != BranchState::Current {
            style.patch(self.merged)
        } else {
            style
        }
    }

    /// The color of the local branch `name`, by what is known about it.
    #[must_use]
    pub fn local(&self, name: &str) -> Style {
        let (state, merged) = self
            .known
            .get(name)
            .copied()
            .unwrap_or((BranchState::Local, false));
        self.style(name, state, merged)
    }

    /// Colors of a remote branch's remote prefix and branch name.
    #[must_use]
    pub fn remote(&self, name: &str) -> (Style, Style) {
        let branch = self
            .rules
            .iter()
            .find(|rule| rule.matches(name, None))
            .map_or(self.remote, |rule| self.remote.patch(rule.style));
        (self.remote, branch)
    }

    /// Color of a ref label on a commit: a local branch, or a `remote/branch`.
    #[must_use]
    pub fn reference(&self, reference: &str) -> Vec<(String, Style)> {
        if self.known.contains_key(reference) {
            return vec![(reference.to_owned(), self.local(reference))];
        }
        match reference.split_once('/') {
            Some((remote, name)) => {
                let (prefix, branch) = self.remote(name);
                vec![(format!("{remote}/"), prefix), (name.to_owned(), branch)]
            }
            None => vec![(reference.to_owned(), self.local(reference))],
        }
    }

    /// What each color means: the configured rules, then the state colors.
    #[must_use]
    pub fn legend(&self, theme: &GitTheme) -> Vec<Line> {
        let swatch = |style: Style, label: &str, meaning: &str| {
            Line::from_spans(vec![
                Span::styled("■ ", style),
                Span::styled(label.to_owned(), style),
                Span::styled(format!("  {meaning}"), theme.muted),
            ])
        };
        let mut lines = Vec::new();
        for rule in self.rules.iter() {
            lines.push(swatch(
                self.states[BranchState::Local.index()].patch(rule.style),
                &rule.description,
                "your rule",
            ));
        }
        for state in BranchState::ALL {
            lines.push(swatch(
                self.states[state.index()],
                state.name(),
                state.meaning(),
            ));
        }
        if self.dim_merged {
            lines.push(swatch(
                self.states[BranchState::Synced.index()].patch(self.merged),
                "dimmed",
                "merged into the base branch",
            ));
        }
        lines
    }
}

/// A color by name (`red`, `bright-cyan`, `bright_cyan`), `#rrggbb`, or theme palette name.
#[must_use]
pub fn parse_color(value: &str, palette: &BTreeMap<String, Color>) -> Option<Color> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |range| u8::from_str_radix(&hex[range], 16).ok();
        return Some(Color::Rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?));
    }
    let named = match value.to_ascii_lowercase().replace('_', "-").as_str() {
        "default" => Some(Color::Default),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "bright-black" | "gray" | "grey" => Some(Color::BrightBlack),
        "bright-red" => Some(Color::BrightRed),
        "bright-green" => Some(Color::BrightGreen),
        "bright-yellow" => Some(Color::BrightYellow),
        "bright-blue" => Some(Color::BrightBlue),
        "bright-magenta" => Some(Color::BrightMagenta),
        "bright-cyan" => Some(Color::BrightCyan),
        "bright-white" => Some(Color::BrightWhite),
        _ => None,
    };
    named.or_else(|| palette.get(value).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(
        name: &str,
        head: bool,
        upstream: bool,
        ahead: u32,
        behind: u32,
        gone: bool,
    ) -> Branch {
        Branch {
            name: name.into(),
            oid: String::new(),
            is_head: head,
            upstream: upstream.then(|| format!("origin/{name}")),
            ahead,
            behind,
            upstream_gone: gone,
            commit_time: 0,
            summary: String::new(),
            worktree: None,
        }
    }

    #[test]
    fn states_follow_head_and_upstream() {
        let cases = [
            (branch("a", true, true, 3, 3, true), BranchState::Current),
            (branch("a", false, true, 0, 0, true), BranchState::Gone),
            (branch("a", false, false, 0, 0, false), BranchState::Local),
            (branch("a", false, true, 0, 0, false), BranchState::Synced),
            (branch("a", false, true, 2, 0, false), BranchState::Ahead),
            (branch("a", false, true, 0, 2, false), BranchState::Behind),
            (branch("a", false, true, 1, 1, false), BranchState::Diverged),
        ];
        for (branch, state) in cases {
            assert_eq!(BranchState::of(&branch), state, "{branch:?}");
        }
    }

    fn compile(rules: &str) -> (BranchColors, Vec<String>, GitTheme) {
        let config: BranchesConfig = toml::from_str(rules).unwrap();
        let theme = GitTheme::fallback();
        let (colors, diagnostics) = BranchColors::new(&config, &theme);
        (colors, diagnostics, theme)
    }

    #[test]
    fn rules_match_in_order_before_state_colors() {
        let (colors, diagnostics, theme) = compile(
            r##"
            colors = [
                { pattern = "^release/", color = "#112233", bold = true },
                { pattern = "^feature/", state = "ahead", color = "magenta" },
                { state = ["behind", "diverged"], color = "bright-red" },
            ]
            "##,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let state = |state: BranchState| theme.branch_states[state as usize];
        // Name rule, whatever the state.
        let release = colors.style("release/1", BranchState::Behind, false);
        assert_eq!(release.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert!(release.modifiers.contains(Modifier::BOLD));
        // Name and state must both hold.
        assert_eq!(
            colors.style("feature/x", BranchState::Ahead, false).fg,
            Some(Color::Magenta)
        );
        assert_eq!(
            colors.style("feature/x", BranchState::Synced, false),
            state(BranchState::Synced)
        );
        // State rule.
        assert_eq!(
            colors.style("x", BranchState::Diverged, false).fg,
            Some(Color::BrightRed)
        );
        // Nothing matches: the state's color.
        assert_eq!(
            colors.style("x", BranchState::Gone, false),
            state(BranchState::Gone)
        );
    }

    #[test]
    fn merged_branches_dim_except_the_current_one() {
        let (colors, _, theme) = compile("");
        let merged = colors.style("x", BranchState::Synced, true);
        assert!(merged.modifiers.contains(Modifier::DIM));
        let current = colors.style("x", BranchState::Current, true);
        assert_eq!(current, theme.branch_states[BranchState::Current as usize]);
        let (off, _, _) = compile("dim_merged = false");
        assert!(
            !off.style("x", BranchState::Synced, true)
                .modifiers
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn local_remote_and_references_share_the_rules() {
        let (colors, _, theme) =
            compile(r#"colors = [{ pattern = "^feature/", color = "magenta" }]"#);
        let colors = colors.with_branches(
            &[
                branch("main", true, true, 0, 0, false),
                branch("feature/x", false, true, 0, 0, false),
                branch("old", false, true, 0, 0, false),
            ],
            &BTreeSet::from(["old".to_owned()]),
        );
        assert_eq!(colors.local("main"), theme.branch_states[0]);
        assert_eq!(colors.local("feature/x").fg, Some(Color::Magenta));
        assert!(colors.local("old").modifiers.contains(Modifier::DIM));
        // A remote branch keeps the remote color for its prefix; its name follows name rules.
        let (prefix, name) = colors.remote("feature/x");
        assert_eq!(prefix, theme.branch_remote);
        assert_eq!(name.fg, Some(Color::Magenta));
        assert_eq!(colors.remote("other").1, theme.branch_remote);
        // Commit labels use the same colors.
        assert_eq!(
            colors.reference("feature/x"),
            vec![("feature/x".to_owned(), colors.local("feature/x"))]
        );
        let remote = colors.reference("origin/feature/x");
        assert_eq!(remote[0], ("origin/".to_owned(), theme.branch_remote));
        assert_eq!(remote[1].1.fg, Some(Color::Magenta));
    }

    #[test]
    fn unusable_rules_are_reported_and_skipped() {
        let (colors, diagnostics, theme) = compile(
            r#"colors = [{ pattern = "(", color = "red" }, { pattern = "x", color = "nope" }]"#,
        );
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(diagnostics[0].contains("branches.colors[0]: invalid pattern"));
        assert!(diagnostics[1].contains("unknown color \"nope\""));
        assert_eq!(
            colors.style("x", BranchState::Local, false),
            theme.branch_states[6]
        );
    }

    #[test]
    fn colors_parse_by_name_hex_and_palette() {
        let palette = BTreeMap::from([("accent".to_owned(), Color::Rgb(1, 2, 3))]);
        assert_eq!(
            parse_color("bright_cyan", &palette),
            Some(Color::BrightCyan)
        );
        assert_eq!(
            parse_color("Bright-Cyan", &palette),
            Some(Color::BrightCyan)
        );
        assert_eq!(
            parse_color("#ff8800", &palette),
            Some(Color::Rgb(255, 136, 0))
        );
        assert_eq!(parse_color("accent", &palette), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(parse_color("#ff88", &palette), None);
        assert_eq!(parse_color("nope", &palette), None);
    }
}
