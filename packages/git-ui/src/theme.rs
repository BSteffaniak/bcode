//! Presentation styles resolved from bcode themes.
//!
//! The UI reads standard semantic roles (`text.*`, `border.*`, `selection.active`, `state.*`,
//! `diff.*`, `syntax.*`) and optional git roles (`git.*`). Missing roles fall back to colorful
//! defaults derived from the theme's state colors so every theme stays readable.

use bcode_syntax_render::SyntaxRole;
use bcode_tui_theme::appearance::{AppearanceResolution, AppearanceSources};
use bcode_tui_theme::definition::{ResolvedThemeDefinition, ThemeSelection};
use bmux_tui::capabilities::TerminalBackground;
use bmux_tui::style::{Color, Modifier, Style};

pub use bcode_tui_theme::appearance::DEFAULT_THEME;

/// Resolved styles for every element the UI paints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct GitTheme {
    pub canvas: Style,
    pub text: Style,
    pub muted: Style,
    pub border: Style,
    pub border_focused: Style,
    pub title: Style,
    pub title_focused: Style,
    pub cursor: Style,
    pub cursor_inactive: Style,
    pub range: Style,
    pub raised: Style,
    pub overlay: Style,
    pub accent: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub info: Style,
    pub added: Style,
    pub removed: Style,
    pub added_row: Style,
    pub removed_row: Style,
    pub added_emphasis: Style,
    pub removed_emphasis: Style,
    /// Flash of a row whose content grew (`git.flash.added`, background).
    pub flash_added: Color,
    /// Flash of a row whose content shrank (`git.flash.removed`, background).
    pub flash_removed: Color,
    pub hunk: Style,
    pub gutter: Style,
    pub staged: Style,
    pub unstaged: Style,
    pub untracked: Style,
    pub conflict: Style,
    pub directory: Style,
    pub branch_head: Style,
    pub branch_local: Style,
    pub branch_remote: Style,
    pub tag: Style,
    pub sha: Style,
    pub author: Style,
    pub date: Style,
    pub key: Style,
    /// Branch name colors by state (see [`crate::branches::BranchState`]), in its order.
    pub branch_states: [Style; 7],
    /// Applied over a branch merged into the base branch.
    pub branch_merged: Style,
    /// Pull request markers by state: open, draft, merged, closed.
    pub pull_states: [Style; 4],
    /// Spinners and progress of running work.
    pub activity: Style,
    /// Colors for identities (graph lanes, authors): distinct from state colors.
    pub graph: Vec<Color>,
    /// Named theme colors, for configuration that refers to them.
    pub palette: std::collections::BTreeMap<String, Color>,
    syntax: [Style; 13],
}

impl GitTheme {
    /// Resolve the theme `sources` select (bcode's selection: configuration under the saved
    /// appearance) for a terminal with `background`. Returns the theme and any diagnostics
    /// ([`Self::from_resolution`]).
    #[must_use]
    pub fn load(
        sources: &AppearanceSources,
        background: TerminalBackground,
    ) -> (Self, Vec<String>) {
        Self::from_resolution(&sources.resolve(background))
    }

    /// The theme of a resolved appearance. A selection that does not resolve falls back to
    /// [`DEFAULT_THEME`] and then to built-in colors; the diagnostics say why, along with saved
    /// choices that could not apply.
    #[must_use]
    pub fn from_resolution(resolution: &AppearanceResolution) -> (Self, Vec<String>) {
        let mut diagnostics: Vec<String> =
            resolution.notices.iter().map(ToString::to_string).collect();
        let theme = match &resolution.resolved {
            Ok(resolved) => Self::from_definition(resolved),
            Err(error) => {
                diagnostics.push(format!("theme {}: {error}", resolution.preferences.name));
                resolution
                    .catalog
                    .resolve(&ThemeSelection::new(DEFAULT_THEME).variant(resolution.variant))
                    .map_or_else(
                        |_| Self::fallback(),
                        |resolved| Self::from_definition(&resolved),
                    )
            }
        };
        (theme, diagnostics)
    }

    /// Built-in colors used when no theme can be resolved.
    #[must_use]
    pub fn fallback() -> Self {
        Self::from_definition(&ResolvedThemeDefinition::empty("fallback", "Fallback"))
    }

    /// Derive UI styles from a resolved theme.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn from_definition(theme: &ResolvedThemeDefinition) -> Self {
        let role = |name: &str, fallback: Style| theme.style(name).unwrap_or(fallback);
        let fg_of = |style: Style, fallback: Color| style.fg.unwrap_or(fallback);

        let canvas = role("canvas", Style::new());
        let text = role("text.primary", Style::new());
        let muted = role("text.muted", Style::new().fg(Color::BrightBlack));
        let success = role("state.success", Style::new().fg(Color::Green));
        let warning = role("state.warning", Style::new().fg(Color::Yellow));
        let error = role("state.error", Style::new().fg(Color::Red));
        let info = role("state.info", Style::new().fg(Color::Cyan));
        let accent_color = theme
            .color("accent")
            .unwrap_or_else(|| fg_of(info, Color::Cyan));
        let accent = Style::new().fg(accent_color);
        let violet = theme.color("violet").unwrap_or(Color::Magenta);
        let green = fg_of(success, Color::Green);
        let red = fg_of(error, Color::Red);
        let yellow = fg_of(warning, Color::Yellow);
        let blue = fg_of(info, Color::Blue);
        let selection = role(
            "selection.active",
            Style::new()
                .bg(Color::BrightBlack)
                .add_modifier(Modifier::BOLD),
        );
        let raised = role("surface.raised", canvas);
        let overlay = role("surface.overlay", raised);
        let border_focused = role("border.focused", accent.add_modifier(Modifier::BOLD));
        let git = |name: &str, fallback: Style| role(&format!("git.{name}"), fallback);
        let added_emphasis = role(
            "diff.added_emphasis",
            Style::new()
                .bg(Color::Rgb(30, 100, 58))
                .add_modifier(Modifier::BOLD),
        );
        let removed_emphasis = role(
            "diff.removed_emphasis",
            Style::new()
                .bg(Color::Rgb(122, 34, 52))
                .add_modifier(Modifier::BOLD),
        );
        let flash = |name: &str, fallback: Style| {
            git(name, Style::new())
                .bg
                .or(fallback.bg)
                .unwrap_or(Color::Default)
        };
        let syntax_role = |name: &str, fallback: Color| {
            role(&format!("syntax.{name}"), Style::new().fg(fallback))
        };
        Self {
            canvas,
            text,
            muted,
            border: role("border.default", muted),
            border_focused,
            title: git("title", muted.add_modifier(Modifier::BOLD)),
            title_focused: git(
                "title.focused",
                Style::new().fg(accent_color).add_modifier(Modifier::BOLD),
            ),
            cursor: git(
                "cursor",
                Style::new()
                    .bg(selection.bg.unwrap_or(Color::BrightBlack))
                    .add_modifier(Modifier::BOLD),
            ),
            cursor_inactive: git(
                "cursor.inactive",
                Style::new().bg(raised.bg.unwrap_or(Color::Black)),
            ),
            range: git(
                "range",
                Style::new().bg(theme.color("selection").unwrap_or(Color::Blue)),
            ),
            raised,
            overlay,
            accent,
            success,
            warning,
            error,
            info,
            added: role("diff.added", Style::new().fg(green)),
            removed: role("diff.removed", Style::new().fg(red)),
            added_row: role(
                "diff.added_row",
                Style::new().bg(theme
                    .color("success_background")
                    .unwrap_or(Color::Rgb(18, 53, 34))),
            ),
            removed_row: role(
                "diff.removed_row",
                Style::new().bg(theme
                    .color("error_background")
                    .unwrap_or(Color::Rgb(63, 20, 32))),
            ),
            flash_added: flash("flash.added", added_emphasis),
            flash_removed: flash("flash.removed", removed_emphasis),
            added_emphasis,
            removed_emphasis,
            hunk: role(
                "diff.hunk",
                Style::new().fg(violet).add_modifier(Modifier::BOLD),
            ),
            gutter: role("source.gutter", muted),
            staged: git("status.staged", Style::new().fg(green)),
            unstaged: git("status.unstaged", Style::new().fg(red)),
            untracked: git("status.untracked", Style::new().fg(yellow)),
            conflict: git(
                "status.conflict",
                Style::new().fg(red).add_modifier(Modifier::BOLD),
            ),
            directory: git(
                "directory",
                Style::new().fg(blue).add_modifier(Modifier::BOLD),
            ),
            branch_head: git(
                "branch.head",
                Style::new().fg(green).add_modifier(Modifier::BOLD),
            ),
            branch_local: git("branch.local", Style::new().fg(accent_color)),
            branch_remote: git("branch.remote", Style::new().fg(violet)),
            tag: git("tag", Style::new().fg(yellow).add_modifier(Modifier::BOLD)),
            sha: git("commit.sha", Style::new().fg(yellow)),
            author: git("commit.author", Style::new().fg(accent_color)),
            date: git("commit.date", muted),
            key: git(
                "key",
                Style::new().fg(accent_color).add_modifier(Modifier::BOLD),
            ),
            branch_states: [
                git(
                    "branch.state.current",
                    Style::new().fg(green).add_modifier(Modifier::BOLD),
                ),
                git("branch.state.synced", Style::new().fg(accent_color)),
                git(
                    "branch.state.ahead",
                    Style::new().fg(theme.color("blue").unwrap_or(Color::Blue)),
                ),
                git("branch.state.behind", Style::new().fg(yellow)),
                git(
                    "branch.state.diverged",
                    Style::new().fg(theme.color("orange").unwrap_or(Color::BrightRed)),
                ),
                git("branch.state.gone", Style::new().fg(red)),
                git("branch.state.local", text),
            ],
            branch_merged: git("branch.merged", Style::new().add_modifier(Modifier::DIM)),
            pull_states: [
                git("pr.open", Style::new().fg(green)),
                git("pr.draft", muted),
                git("pr.merged", Style::new().fg(violet)),
                git("pr.closed", Style::new().fg(red)),
            ],
            activity: git(
                "activity",
                Style::new().fg(accent_color).add_modifier(Modifier::BOLD),
            ),
            // Bright variants, so a lane or an author never reads as a branch state, tag or
            // remote.
            graph: vec![
                Color::BrightBlue,
                Color::BrightMagenta,
                Color::BrightCyan,
                Color::BrightGreen,
                Color::BrightWhite,
            ],
            palette: theme.palette.clone(),
            syntax: [
                syntax_role("text", text.fg.unwrap_or(Color::Default)),
                syntax_role("comment", Color::BrightBlack).add_modifier(Modifier::ITALIC),
                syntax_role("keyword", Color::Magenta),
                syntax_role("function", Color::Blue),
                syntax_role("variable", Color::Cyan),
                syntax_role("string", Color::Green),
                syntax_role("number", Color::Yellow),
                syntax_role("type", Color::Yellow),
                syntax_role("operator", Color::Cyan),
                syntax_role("punctuation", text.fg.unwrap_or(Color::Default)),
                role(
                    "markdown.heading",
                    Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                ),
                role("markdown.link", Style::new().fg(Color::Cyan)),
                role("markdown.inline_code", Style::new().fg(Color::Yellow)),
            ],
        }
    }

    /// Style for a syntax token role.
    #[must_use]
    pub const fn syntax(&self, role: SyntaxRole) -> Style {
        self.syntax[match role {
            SyntaxRole::Text => 0,
            SyntaxRole::Comment => 1,
            SyntaxRole::Keyword => 2,
            SyntaxRole::Function => 3,
            SyntaxRole::Variable => 4,
            SyntaxRole::String => 5,
            SyntaxRole::Number => 6,
            SyntaxRole::Type => 7,
            SyntaxRole::Operator => 8,
            SyntaxRole::Punctuation => 9,
            SyntaxRole::Heading => 10,
            SyntaxRole::Link => 11,
            SyntaxRole::Raw => 12,
        }]
    }

    /// Stable color for a graph lane.
    #[must_use]
    pub fn lane(&self, lane: usize) -> Style {
        Style::new().fg(self.graph[lane % self.graph.len()])
    }

    /// Stable identity color for a name, so each author keeps a color.
    #[must_use]
    pub fn hashed(&self, name: &str) -> Style {
        let hash = name.bytes().fold(0usize, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(usize::from(byte))
        });
        self.lane(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_theme_distinguishes_additions_from_removals() {
        let catalog = bcode_tui_theme::definition::ThemeCatalog::bundled().unwrap();
        for definition in catalog
            .definitions()
            .filter(|definition| definition.kind() == bcode_tui_theme::definition::ThemeKind::Theme)
        {
            let resolved = catalog
                .resolve(&ThemeSelection::new(definition.id()))
                .unwrap();
            let theme = GitTheme::from_definition(&resolved);
            assert_ne!(theme.added, theme.removed, "{}", definition.id());
            assert!(theme.graph.len() > 1);
        }
    }

    fn sources(
        dir: &std::path::Path,
        configured: &str,
        saved: bcode_tui_theme::appearance::SavedAppearance,
    ) -> AppearanceSources {
        AppearanceSources {
            configured: bcode_tui_theme::appearance::ThemePreferences {
                name: configured.to_owned(),
                ..bcode_tui_theme::appearance::ThemePreferences::default()
            },
            saved,
            watched_files: Vec::new(),
            user_config_dir: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            ..AppearanceSources::default()
        }
    }

    #[test]
    fn missing_theme_falls_back_with_diagnostic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (theme, diagnostics) = GitTheme::load(
            &sources(
                dir.path(),
                "does-not-exist",
                bcode_tui_theme::appearance::SavedAppearance::default(),
            ),
            TerminalBackground::Unknown,
        );
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("does-not-exist"))
        );
        assert_eq!(theme.syntax(SyntaxRole::Text), theme.syntax[0]);
    }

    #[test]
    fn the_palette_saved_by_the_appearance_picker_is_presented() {
        let dir = tempfile::tempdir().expect("tempdir");
        let saved = bcode_tui_theme::appearance::SavedAppearance {
            theme: Some("catppuccin-mocha".to_owned()),
            profile: None,
        };
        let (picked, diagnostics) = GitTheme::load(
            &sources(dir.path(), "nord", saved),
            TerminalBackground::Unknown,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let expected = GitTheme::from_definition(
            &bcode_tui_theme::definition::ThemeCatalog::bundled()
                .expect("bundled")
                .resolve(
                    &ThemeSelection::new("catppuccin-mocha")
                        .variant(bcode_tui_theme::definition::ResolvedThemeVariant::Dark),
                )
                .expect("resolves"),
        );
        assert_eq!(picked, expected);
    }
}
