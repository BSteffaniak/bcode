//! End-to-end behavior: keystrokes drive real git operations in temporary repositories.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bcode_git_ui::app::{App, View, Window};
use bcode_git_ui::config::LoadedConfig;
use bcode_git_ui::{RunOptions, build_app, render_text};
use bmux_tui::event::Event;
use bmux_tui_runtime::{Program, RuntimeEvent};
use tempfile::TempDir;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Harness {
    dir: TempDir,
    state: TempDir,
    app: App,
}

impl Harness {
    fn new(setup: impl FnOnce(&Path)) -> Self {
        Self::with_config(setup, LoadedConfig::default())
    }

    fn with_config(setup: impl FnOnce(&Path), config: LoadedConfig) -> Self {
        Self::with_options(setup, config, None)
    }

    fn with_options(
        setup: impl FnOnce(&Path),
        mut config: LoadedConfig,
        commit_message: Option<String>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "--quiet", "--initial-branch", "main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        setup(path);
        config.ui.refresh.watch = false;
        config.ui.refresh.poll_seconds = 0;
        let state = tempfile::tempdir().unwrap();
        let app = launch(path, config, commit_message, state.path());
        let mut harness = Self { dir, state, app };
        harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
        harness
    }

    /// Start the client again from `cwd` (inside the same repository), with the same
    /// remembered state.
    fn relaunch_in(&mut self, cwd: &Path) {
        let mut config = LoadedConfig::default();
        config.ui.refresh.watch = false;
        config.ui.refresh.poll_seconds = 0;
        self.app = launch(cwd, config, None, self.state.path());
        self.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    }

    fn relaunch(&mut self) {
        let path = self.dir.path().to_owned();
        self.relaunch_in(&path);
    }

    /// Quit with `q`, then start again in the same directory with `config`.
    fn quit_and_relaunch(&mut self, config: LoadedConfig) {
        self.keys(&["q"]);
        let mut config = config;
        config.ui.refresh.watch = false;
        config.ui.refresh.poll_seconds = 0;
        self.app = launch(self.dir.path(), config, None, self.state.path());
        self.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    }

    fn region(&self, id: &str) -> bmux_tui::geometry::Rect {
        self.app
            .hit_regions()
            .into_iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, area)| area)
            .unwrap_or_else(|| panic!("no region {id}"))
    }

    fn mouse(&mut self, kind: bmux_tui::event::MouseEventKind, x: u16, y: u16) {
        self.send(RuntimeEvent::Terminal(Event::Mouse(
            bmux_tui::event::MouseEvent::new(kind, bmux_tui::geometry::Point::new(x, y)),
        )));
    }

    fn click(&mut self, id: &str) {
        let area = self.region(id);
        self.mouse(
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            area.x,
            area.y,
        );
        self.mouse(
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
            area.x,
            area.y,
        );
    }

    /// Drag the handle with this hit id by `dx`, `dy` cells.
    fn drag(&mut self, id: &str, dx: i32, dy: i32) {
        use bmux_tui::event::{MouseButton, MouseEventKind};
        let area = self.region(id);
        let (x, y) = (area.x, area.y);
        let to = |value: u16, delta: i32| u16::try_from(i32::from(value) + delta).unwrap();
        self.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        self.mouse(
            MouseEventKind::Drag(MouseButton::Left),
            to(x, dx),
            to(y, dy),
        );
        self.mouse(MouseEventKind::Up(MouseButton::Left), to(x, dx), to(y, dy));
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn send(&mut self, event: RuntimeEvent<bcode_git_ui::app::Msg>) {
        self.app.update(event).unwrap();
        self.app.drain_inline().unwrap();
    }

    fn keys(&mut self, keys: &[&str]) {
        for key in keys {
            // Mirror terminal decoding, which reports the space bar as a character.
            let stroke = if *key == "space" {
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char(' '))
            } else {
                bmux_keyboard::parse_key_stroke(key).unwrap()
            };
            self.send(RuntimeEvent::Terminal(Event::Key(stroke)));
        }
    }

    fn paste(&mut self, text: &str) {
        self.send(RuntimeEvent::Terminal(Event::Paste(text.to_owned())));
    }

    fn screen(&mut self, width: u16, height: u16) -> String {
        render_text(&mut self.app, width, height).join("\n")
    }

    fn log(&self) -> Vec<String> {
        git(self.path(), &["log", "--format=%s"])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn launch(path: &Path, config: LoadedConfig, commit_message: Option<String>, state: &Path) -> App {
    let mut app = build_app(RunOptions {
        path: path.to_owned(),
        config,
        host: Arc::new(bcode_git_ui::host::StandaloneHost),
        commit_message,
        state_dir: Some(state.to_owned()),
    })
    .unwrap();
    app.set_inline(true);
    app
}

fn commit(path: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(path.join(file), content).unwrap();
    git(path, &["add", "--all"]);
    git(path, &["commit", "--quiet", "-m", message]);
}

#[test]
fn renders_panels_and_highlighted_file_diff() {
    let mut harness = Harness::new(|path| {
        commit(path, "main.rs", "fn main() {}\n", "init");
        std::fs::write(
            path.join("main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
    });
    let screen = harness.screen(140, 40);
    assert!(screen.contains("[1]"), "{screen}");
    assert!(screen.contains("[2] Files"), "{screen}");
    assert!(screen.contains("[3] Local"), "{screen}");
    assert!(screen.contains("[4] Commits"), "{screen}");
    assert!(screen.contains("main.rs"), "{screen}");
    assert!(screen.contains("Unstaged changes"), "{screen}");
    assert!(screen.contains("println!(\"hi\");"), "{screen}");
    assert!(screen.contains("init"), "{screen}");
}

#[test]
fn stages_and_commits_with_keys() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        std::fs::write(path.join("a.txt"), "b\n").unwrap();
    });
    harness.keys(&["space"]);
    assert_eq!(
        git(harness.path(), &["diff", "--cached", "--name-only"]),
        "a.txt\n"
    );
    harness.keys(&["c"]);
    harness.paste("Update a");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["Update a", "init"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("Working tree clean"), "{screen}");
}

#[test]
fn commit_editor_opens_on_a_clean_tree_and_reports_on_submit() {
    let mut harness = Harness::new(|path| commit(path, "a.txt", "a\n", "init"));
    harness.keys(&["c"]);
    harness.paste("Nothing yet");
    let screen = harness.screen(120, 30);
    assert!(screen.contains("Commit message"), "{screen}");
    assert!(screen.contains("amend: off"), "{screen}");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["init"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("Nothing to commit"), "{screen}");
    // The editor stays open with the message intact; changes made meanwhile can be committed.
    assert!(screen.contains("Nothing yet"), "{screen}");
    std::fs::write(harness.path().join("a.txt"), "b\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["Nothing yet", "init"]);
}

#[test]
fn stages_a_single_line_in_the_staging_view() {
    let mut harness = Harness::new(|path| {
        commit(path, "f.txt", "one\ntwo\nthree\n", "init");
        std::fs::write(path.join("f.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    });
    // Enter staging; the cursor snaps to the first change (removal of "two").
    harness.keys(&["enter", "j", "space"]);
    let staged = git(harness.path(), &["show", ":f.txt"]);
    assert_eq!(staged, "one\ntwo\nTWO\nthree\n");
    let screen = harness.screen(120, 30);
    assert!(screen.contains("Staged changes"), "{screen}");
    assert!(screen.contains("[LINE]"), "{screen}");
}

#[test]
fn creates_and_checks_out_a_branch() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&["3", "n"]);
    harness.paste("feature/x");
    harness.keys(&["enter"]);
    assert_eq!(
        git(harness.path(), &["branch", "--show-current"]).trim(),
        "feature/x"
    );
    let screen = harness.screen(120, 30);
    assert!(screen.contains("feature/x"), "{screen}");
}

#[test]
fn squashes_a_commit_into_its_parent() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "first");
        commit(path, "b", "b", "second");
        commit(path, "c", "c", "third");
    });
    harness.keys(&["4", "s", "enter"]);
    assert_eq!(harness.log().len(), 2);
    assert!(harness.path().join("c").exists());
}

#[test]
fn moves_commits_and_drops_them() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "first");
        commit(path, "b", "b", "second");
        commit(path, "c", "c", "third");
    });
    harness.keys(&["4", "ctrl+j"]);
    assert_eq!(harness.log(), ["second", "third", "first"]);
    harness.keys(&["k", "d", "enter"]);
    assert_eq!(harness.log(), ["third", "first"]);
}

#[test]
fn stashes_and_pops() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a\n", "init");
        std::fs::write(path.join("a"), "changed\n").unwrap();
    });
    harness.keys(&["s"]);
    harness.paste("wip");
    harness.keys(&["enter"]);
    assert_eq!(
        std::fs::read_to_string(harness.path().join("a")).unwrap(),
        "a\n"
    );
    harness.keys(&["5", "g"]);
    assert_eq!(
        std::fs::read_to_string(harness.path().join("a")).unwrap(),
        "changed\n"
    );
}

#[test]
fn undo_returns_head_to_before_a_commit() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a\n", "init");
        std::fs::write(path.join("a"), "b\n").unwrap();
    });
    harness.keys(&["space", "c"]);
    harness.paste("second");
    harness.keys(&["enter"]);
    assert_eq!(harness.log().len(), 2);
    harness.keys(&["z", "enter"]);
    assert_eq!(harness.log(), ["init"]);
    harness.keys(&["ctrl+z", "enter"]);
    assert_eq!(harness.log(), ["second", "init"]);
}

#[test]
fn keybinding_overrides_change_dispatch() {
    let config = LoadedConfig {
        keybindings: BTreeMap::from([(
            "git.files".to_owned(),
            BTreeMap::from([
                ("x".to_owned(), "bcode.git.files.toggle-staged".to_owned()),
                ("space".to_owned(), String::new()),
            ]),
        )]),
        ..LoadedConfig::default()
    };
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a\n", "init");
            std::fs::write(path.join("a"), "b\n").unwrap();
        },
        config,
    );
    harness.keys(&["space"]);
    assert_eq!(
        git(harness.path(), &["diff", "--cached", "--name-only"]),
        ""
    );
    harness.keys(&["x"]);
    assert_eq!(
        git(harness.path(), &["diff", "--cached", "--name-only"]),
        "a\n"
    );
}

#[test]
fn custom_commands_run_with_placeholders() {
    let mut config = LoadedConfig::default();
    config.ui.custom_commands = vec![bcode_git_ui::config::CustomCommand {
        key: "ctrl+b".into(),
        scope: "git.files".into(),
        command: "cp {file} {file}.bak".into(),
        description: Some("Back up file".into()),
        prompts: Vec::new(),
        confirm: false,
        interactive: false,
        show_output: false,
    }];
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a\n", "init");
            std::fs::write(path.join("a"), "b\n").unwrap();
        },
        config,
    );
    harness.keys(&["ctrl+b"]);
    assert_eq!(
        std::fs::read_to_string(harness.path().join("a.bak")).unwrap(),
        "b\n"
    );
}

#[test]
fn renders_at_tiny_and_narrow_sizes_with_wide_characters() {
    let mut harness = Harness::new(|path| {
        commit(path, "readme", "x\n", "初期コミット 🎉");
        std::fs::write(path.join("日本語ファイル名.txt"), "内容 e\u{301}\n").unwrap();
    });
    let tiny = harness.screen(12, 4);
    assert!(tiny.contains("bcode git"), "{tiny}");
    for (width, height) in [(30, 10), (60, 20), (83, 24), (200, 60)] {
        let screen = harness.screen(width, height);
        assert!(!screen.is_empty());
    }
    let wide = harness.screen(160, 40);
    assert!(wide.contains("日本語ファイル名.txt"), "{wide}");
    assert!(wide.contains("初期コミット"), "{wide}");
}

#[test]
fn help_and_palette_open_and_run_actions() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&["?"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Keybindings"), "{screen}");
    harness.keys(&["escape", "ctrl+p"]);
    harness.paste("new branch");
    harness.keys(&["enter"]);
    harness.paste("from-palette");
    harness.keys(&["enter"]);
    assert_eq!(
        git(harness.path(), &["branch", "--show-current"]).trim(),
        "from-palette"
    );
}

#[test]
fn which_key_popup_shows_sequence_continuations() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&["3", "g"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Continue with"), "{screen}");
    harness.keys(&["g"]);
    let screen = harness.screen(140, 40);
    assert!(!screen.contains("Continue with"), "{screen}");
}

#[test]
fn mouse_clicks_focus_and_select_rows() {
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;

    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "first");
        commit(path, "b", "b", "second");
        commit(path, "c", "c", "third");
        std::fs::write(path.join("a"), "changed").unwrap();
    });
    harness.screen(140, 40);
    let (_, area) = harness
        .app
        .hit_regions()
        .into_iter()
        .find(|(id, _)| id == "side-4.2")
        .expect("third commit row is interactive");
    let click = |kind| {
        RuntimeEvent::Terminal(Event::Mouse(MouseEvent::new(
            kind,
            Point::new(area.x + 2, area.y),
        )))
    };
    harness.send(click(MouseEventKind::Down(MouseButton::Left)));
    assert_eq!(harness.app.focused_window(), Window::Side(3));
    assert_eq!(harness.app.view_cursor(View::Commits), 2);
    harness.screen(140, 40);
    // The wheel scrolls the page; it does not select.
    harness.send(click(MouseEventKind::ScrollUp));
    assert_eq!(harness.app.view_cursor(View::Commits), 2);
}

#[test]
fn resolves_a_merge_conflict_and_continues() {
    let mut harness = Harness::new(|path| {
        commit(path, "f", "base\n", "base");
        git(path, &["checkout", "--quiet", "-b", "feature"]);
        commit(path, "f", "feature\n", "feature change");
        git(path, &["checkout", "--quiet", "main"]);
        commit(path, "f", "main\n", "main change");
    });
    // Branches panel: filter to "feature" and merge it.
    harness.keys(&["3", "/"]);
    harness.paste("feature");
    harness.keys(&["enter", "shift+m", "m"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("MERGING"), "{screen}");
    // Take theirs for the conflicted file, then continue from the merge menu.
    harness.keys(&["2", "ctrl+t", "m", "c"]);
    assert_eq!(
        std::fs::read_to_string(harness.path().join("f")).unwrap(),
        "feature\n"
    );
    assert!(!harness.path().join(".git/MERGE_HEAD").exists());
    assert_eq!(harness.log().len(), 4);
}

#[test]
fn pushes_with_upstream_to_a_new_remote() {
    let remote = tempfile::tempdir().unwrap();
    git(
        remote.path(),
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    let url = remote.path().to_string_lossy().into_owned();
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "init");
        git(path, &["remote", "add", "origin", &url]);
    });
    harness.keys(&["shift+p", "1"]);
    assert_eq!(
        git(remote.path(), &["log", "--format=%s", "main"]).trim(),
        "init"
    );
    assert_eq!(
        git(
            harness.path(),
            &["rev-parse", "--abbrev-ref", "@{upstream}"]
        )
        .trim(),
        "origin/main"
    );
}

#[test]
fn copies_and_pastes_commits_with_cherry_pick() {
    let mut harness = Harness::new(|path| {
        commit(path, "base", "base", "base");
        git(path, &["checkout", "--quiet", "-b", "other"]);
        commit(path, "picked", "picked", "pick me");
        git(path, &["checkout", "--quiet", "main"]);
    });
    // View "other" branch commits, copy the top commit, go back and paste onto main.
    harness.keys(&[
        "3", "j", "enter", "shift+c", "escape", "4", "shift+v", "enter",
    ]);
    assert_eq!(harness.log(), ["pick me", "base"]);
}

#[test]
fn rewords_an_older_commit() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "first");
        commit(path, "b", "b", "second");
    });
    harness.keys(&["4", "j", "r"]);
    // Replace the prefilled message.
    for _ in 0.."first".len() {
        harness.keys(&["backspace"]);
    }
    harness.paste("renamed");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["second", "renamed"]);
}

#[test]
fn filters_lists_and_drills_into_commit_files() {
    let mut harness = Harness::new(|path| {
        commit(path, "alpha.rs", "a", "add alpha");
        commit(path, "beta.rs", "b", "add beta");
        commit(path, "gamma.rs", "c", "add gamma");
    });
    harness.keys(&["4", "/"]);
    harness.paste("beta");
    harness.keys(&["enter"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("add beta"), "{screen}");
    assert!(!screen.contains("✱ main add gamma"), "{screen}");
    harness.keys(&["enter"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Files ·"), "{screen}");
    assert!(screen.contains("beta.rs"), "{screen}");
    harness.keys(&["escape", "escape"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("✱ main add gamma"), "{screen}");
}

fn configured(toml: &str) -> LoadedConfig {
    let loaded = bcode_git_ui::config::from_table(&toml.parse().unwrap());
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    loaded
}

fn line_with<'a>(screen: &'a str, needle: &str) -> &'a str {
    screen
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no line contains {needle:?}:\n{screen}"))
}

#[test]
fn half_mode_shows_one_side_window_beside_the_changes() {
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;

    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a.txt", "a\n", "first commit");
            std::fs::write(path.join("a.txt"), "changed\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.layout]
            screen_mode = "half"
            half_side_panel_width = 40
            "#,
        ),
    );
    let screen = harness.screen(120, 30);
    // Files is the only framed side window; the others are single title rows.
    assert!(line_with(&screen, "[2] Files").contains('┌'), "{screen}");
    assert!(!line_with(&screen, "[4] Commits").contains('┌'), "{screen}");
    assert!(!screen.contains("first commit"), "{screen}");
    assert!(screen.contains("Unstaged changes"), "{screen}");

    // Clicking a collapsed title focuses that window, which then expands.
    let area = harness
        .app
        .hit_regions()
        .into_iter()
        .find(|(id, _)| id == "pane-4.title")
        .map(|(_, area)| area)
        .expect("collapsed commits title is clickable");
    harness.send(RuntimeEvent::Terminal(Event::Mouse(MouseEvent::new(
        MouseEventKind::Down(MouseButton::Left),
        Point::new(area.x + 1, area.y),
    ))));
    assert_eq!(harness.app.focused_window(), Window::Side(3));
    let screen = harness.screen(120, 30);
    assert!(screen.contains("first commit"), "{screen}");
    assert!(!line_with(&screen, "[2] Files").contains('┌'), "{screen}");
}

#[test]
fn starts_in_the_configured_window_with_a_flat_file_list() {
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a.txt", "a\n", "init");
            std::fs::create_dir_all(path.join("src/deep")).unwrap();
            std::fs::write(path.join("src/deep/lib.rs"), "fn x() {}\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.layout]
            initial_window = "commits"
            [plugins.config."bcode.git".ui.files]
            tree = false
            "#,
        ),
    );
    assert_eq!(harness.app.focused_window(), Window::Side(3));
    let screen = harness.screen(120, 30);
    assert!(screen.contains("src/deep/lib.rs"), "{screen}");
}

#[test]
fn regrouped_windows_follow_the_configured_order() {
    let mut harness = Harness::with_config(
        |path| commit(path, "a.txt", "a\n", "init"),
        configured(
            r#"
            [plugins.config."bcode.git".ui.layout]
            windows = [["files", "stash"], ["commits", "branches"]]
            "#,
        ),
    );
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[1] Files · Stash"), "{screen}");
    assert!(screen.contains("[2] Commits · Local"), "{screen}");
    assert!(!screen.contains("Remotes"), "{screen}");
    harness.keys(&["2"]);
    assert_eq!(harness.app.focused_window(), Window::Side(1));
    // There is no third window; the key does nothing.
    harness.keys(&["3"]);
    assert_eq!(harness.app.focused_window(), Window::Side(1));
    harness.keys(&["]"]);
    assert_eq!(harness.app.current_view(), View::Branches);
}

#[test]
fn staged_and_unstaged_sections_sit_side_by_side_when_configured() {
    let setup = |path: &Path| {
        commit(path, "f.txt", "one\ntwo\n", "init");
        std::fs::write(path.join("f.txt"), "ONE\ntwo\n").unwrap();
        git(path, &["add", "f.txt"]);
        std::fs::write(path.join("f.txt"), "ONE\nTWO\n").unwrap();
    };
    let mut beside = Harness::with_config(
        setup,
        configured("[plugins.config.\"bcode.git\".ui.layout]\nmain_split = \"side-by-side\""),
    );
    let screen = beside.screen(140, 30);
    assert!(
        line_with(&screen, "Unstaged changes").contains("Staged changes"),
        "{screen}"
    );
    let mut stacked = Harness::with_config(
        setup,
        configured("[plugins.config.\"bcode.git\".ui.layout]\nmain_split = \"stacked\""),
    );
    let screen = stacked.screen(140, 30);
    assert!(
        !line_with(&screen, "Unstaged changes").contains("Staged changes"),
        "{screen}"
    );
}

#[test]
fn commit_editor_floats_over_a_visible_screen() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "earlier work");
        std::fs::write(path.join("a.txt"), "b\n").unwrap();
    });
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Summary"), "{screen}");
    assert!(screen.contains("Description"), "{screen}");
    // Panels around the popup are still drawn.
    assert!(screen.contains("[2] Files"), "{screen}");
    assert!(screen.contains("earlier work"), "{screen}");
}

#[test]
fn summary_and_description_form_one_commit_message() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        std::fs::write(path.join("a.txt"), "b\n").unwrap();
    });
    harness.keys(&["c"]);
    harness.paste("Add the thing");
    harness.keys(&["tab"]);
    harness.paste("First detail");
    // Enter adds a line in the description instead of committing.
    harness.keys(&["enter"]);
    harness.paste("Second detail");
    assert_eq!(harness.log(), ["init"]);
    harness.keys(&["alt+enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "Add the thing\n\nFirst detail\nSecond detail\n\n"
    );
}

#[test]
fn pasted_message_splits_into_summary_and_description() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        std::fs::write(path.join("a.txt"), "b\n").unwrap();
    });
    harness.keys(&["c"]);
    harness.paste("Subject line\n\nBody text");
    harness.keys(&["tab", "enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "Subject line\n\nBody text\n\n"
    );
}

#[test]
fn description_alone_is_not_a_commit_message() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        std::fs::write(path.join("a.txt"), "b\n").unwrap();
    });
    harness.keys(&["c", "tab"]);
    harness.paste("Only details");
    harness.keys(&["alt+enter"]);
    assert_eq!(harness.log(), ["init"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("A summary is required"), "{screen}");
}

#[test]
fn reword_keeps_an_existing_description() {
    let mut harness = Harness::new(|path| {
        std::fs::write(path.join("a.txt"), "a\n").unwrap();
        git(path, &["add", "--all"]);
        git(
            path,
            &["commit", "--quiet", "-m", "Title", "-m", "Body line"],
        );
    });
    harness.keys(&["4", "r"]);
    let screen = harness.screen(140, 40);
    assert!(line_with(&screen, "Title").contains('│'), "{screen}");
    assert!(
        !line_with(&screen, "Body line").contains("Title"),
        "{screen}"
    );
    harness.keys(&["end"]);
    harness.paste(" v2");
    harness.keys(&["enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "Title v2\n\nBody line\n\n"
    );
}

fn changed_repo(path: &Path) {
    commit(path, "a.txt", "a\n", "init");
    std::fs::write(path.join("a.txt"), "b\n").unwrap();
}

fn git_file(harness: &Harness, name: &str) -> Option<String> {
    std::fs::read_to_string(harness.path().join(".git").join(name)).ok()
}

#[test]
fn environment_message_wins_once_then_drafts_take_over() {
    let mut harness = Harness::with_options(
        |path| {
            changed_repo(path);
            std::fs::write(path.join(".git/BCODE_GIT_COMMIT_MSG"), "Saved draft\n").unwrap();
        },
        LoadedConfig::default(),
        Some("From the environment".into()),
    );
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from $BCODE_GIT_COMMIT_MSG"), "{screen}");
    assert!(screen.contains("From the environment"), "{screen}");
    // Cancelling keeps the environment message as the draft, since it cannot be read again.
    harness.keys(&["escape"]);
    assert_eq!(
        git_file(&harness, "BCODE_GIT_COMMIT_MSG").as_deref(),
        Some("From the environment")
    );
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from BCODE_GIT_COMMIT_MSG"), "{screen}");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["From the environment", "init"]);
    // The committed draft is gone, so the next editor starts empty.
    assert_eq!(git_file(&harness, "BCODE_GIT_COMMIT_MSG"), None);
}

#[test]
fn lazygit_draft_is_used_and_cleared_only_by_a_commit_that_used_it() {
    let mut harness = Harness::new(|path| {
        changed_repo(path);
        std::fs::write(
            path.join(".git/LAZYGIT_PENDING_COMMIT"),
            "From lazygit\n\nWith body",
        )
        .unwrap();
    });
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from LAZYGIT_PENDING_COMMIT"), "{screen}");
    assert!(screen.contains("With body"), "{screen}");
    harness.keys(&["alt+enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "From lazygit\n\nWith body\n\n"
    );
    assert_eq!(git_file(&harness, "LAZYGIT_PENDING_COMMIT"), None);
}

#[test]
fn bcode_draft_outranks_lazygit_and_leaves_its_file_alone() {
    let mut harness = Harness::new(|path| {
        changed_repo(path);
        std::fs::write(path.join(".git/BCODE_GIT_COMMIT_MSG"), "From bcode").unwrap();
        std::fs::write(path.join(".git/LAZYGIT_PENDING_COMMIT"), "From lazygit").unwrap();
    });
    harness.keys(&["c", "enter"]);
    assert_eq!(harness.log(), ["From bcode", "init"]);
    assert_eq!(
        git_file(&harness, "LAZYGIT_PENDING_COMMIT").as_deref(),
        Some("From lazygit")
    );
}

#[test]
fn a_draft_replacing_merge_msg_is_announced_and_can_be_cleared() {
    let mut harness = Harness::new(|path| {
        changed_repo(path);
        std::fs::write(
            path.join(".git/MERGE_MSG"),
            "Merge branch 'x'\n# Conflicts:\n",
        )
        .unwrap();
        std::fs::write(path.join(".git/BCODE_GIT_COMMIT_MSG"), "Old draft").unwrap();
    });
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("replaces MERGE_MSG"), "{screen}");
    harness.keys(&["ctrl+l"]);
    let screen = harness.screen(140, 40);
    assert!(!screen.contains("Old draft"), "{screen}");
    // Leaving with an empty editor removes the draft, so git's message shows next time.
    harness.keys(&["escape"]);
    assert_eq!(git_file(&harness, "BCODE_GIT_COMMIT_MSG"), None);
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from MERGE_MSG"), "{screen}");
    assert!(screen.contains("Merge branch 'x'"), "{screen}");
    assert!(!screen.contains("# Conflicts"), "{screen}");
    // Cancelling untouched git text does not turn it into a draft.
    harness.keys(&["escape"]);
    assert_eq!(git_file(&harness, "BCODE_GIT_COMMIT_MSG"), None);
}

#[test]
fn a_rejected_commit_keeps_its_message_as_the_draft() {
    let mut harness = Harness::new(|path| {
        changed_repo(path);
        // A user's global hooksPath would otherwise bypass this repository's hooks.
        std::fs::create_dir_all(path.join("hooks")).unwrap();
        git(path, &["config", "core.hooksPath", "hooks"]);
        let hook = path.join("hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    });
    harness.keys(&["c"]);
    harness.paste("Blocked by hook");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["init"]);
    assert_eq!(
        git_file(&harness, "BCODE_GIT_COMMIT_MSG").as_deref(),
        Some("Blocked by hook")
    );
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Blocked by hook"), "{screen}");
}

#[test]
fn configured_sources_limit_and_order_the_prefill() {
    let mut harness = Harness::with_config(
        |path| {
            changed_repo(path);
            std::fs::write(path.join(".git/LAZYGIT_PENDING_COMMIT"), "From lazygit").unwrap();
            std::fs::write(path.join(".git/MERGE_MSG"), "Merge message").unwrap();
        },
        configured(
            "[plugins.config.\"bcode.git\".ui.commit]\nsources = [\"merge\", \"bcode-draft\"]",
        ),
    );
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Merge message"), "{screen}");
    assert!(!screen.contains("From lazygit"), "{screen}");
}

/// No cell of the open modal, from its `╭` corner to its `╯` corner, falls back to the
/// terminal's default background (highlighted rows may use their own color).
fn assert_solid_modal(harness: &mut Harness) {
    use bmux_tui::geometry::Point;

    let (width, height) = (140u16, 40u16);
    let buffer = bcode_git_ui::render_buffer(&mut harness.app, width, height);
    let find = |corner: &str| {
        (0..height).find_map(|y| {
            (0..width).find_map(|x| {
                (buffer.get(Point::new(x, y)).unwrap().symbol == corner).then_some((x, y))
            })
        })
    };
    let (left, top) = find("╭").expect("a modal is open");
    let (right, bottom) = find("╯").expect("a modal is open");
    for y in top..=bottom {
        for x in left..=right {
            let cell = buffer.get(Point::new(x, y)).unwrap();
            assert!(
                cell.style.bg.is_some(),
                "cell ({x}, {y}) {:?} has no background in row {:?}",
                cell.symbol,
                buffer.row_symbols(y)
            );
        }
    }
}

#[test]
fn modals_have_a_solid_background() {
    let mut harness = Harness::new(changed_repo);
    harness.keys(&["c"]);
    harness.paste("Typed summary");
    assert_solid_modal(&mut harness);
    harness.keys(&["tab"]);
    harness.paste("Typed description");
    assert_solid_modal(&mut harness);
    harness.keys(&["escape", "?"]);
    assert_solid_modal(&mut harness);
    harness.keys(&["escape", "ctrl+p"]);
    harness.paste("com");
    assert_solid_modal(&mut harness);
}

fn sources_changed(harness: &mut Harness) {
    harness.send(RuntimeEvent::Message(
        bcode_git_ui::app::Msg::CommitSourcesChanged,
    ));
}

#[test]
fn an_empty_commit_editor_takes_messages_written_while_open() {
    let mut harness = Harness::new(changed_repo);
    harness.keys(&["c"]);
    std::fs::write(
        harness.path().join(".git/BCODE_GIT_COMMIT_MSG"),
        "Written meanwhile\n\nWith details",
    )
    .unwrap();
    sources_changed(&mut harness);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from BCODE_GIT_COMMIT_MSG"), "{screen}");
    assert!(screen.contains("With details"), "{screen}");
    harness.keys(&["enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "Written meanwhile\n\nWith details\n\n"
    );
}

#[test]
fn typed_text_is_never_replaced_but_an_emptied_editor_is_filled() {
    let mut harness = Harness::new(changed_repo);
    harness.keys(&["c"]);
    harness.paste("Mine");
    std::fs::write(harness.path().join(".git/MERGE_MSG"), "From git").unwrap();
    sources_changed(&mut harness);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Mine"), "{screen}");
    assert!(!screen.contains("From git"), "{screen}");
    // Typing and then deleting everything leaves an editor that accepts a prefill again.
    harness.keys(&["backspace", "backspace", "backspace", "backspace"]);
    sources_changed(&mut harness);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("From git"), "{screen}");
    assert!(screen.contains("from MERGE_MSG"), "{screen}");
}

#[test]
fn source_watching_follows_the_commit_editor() {
    let mut harness = Harness::new(changed_repo);
    let key = |key: &str| {
        RuntimeEvent::Terminal(Event::Key(bmux_keyboard::parse_key_stroke(key).unwrap()))
    };
    let opened = harness.app.update(key("c")).unwrap();
    assert_eq!(opened.subscriptions.len(), 1);
    let typed = harness.app.update(key("x")).unwrap();
    assert!(typed.subscriptions.is_empty() && typed.cancelled_subscriptions.is_empty());
    let closed = harness.app.update(key("escape")).unwrap();
    assert_eq!(
        closed
            .cancelled_subscriptions
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>(),
        [bcode_git_ui::commit_message::WATCH_KEY]
    );
}

#[test]
fn long_diff_lines_wrap_with_a_blank_gutter_and_stage_as_one_line() {
    let long = "let value = alpha + bravo + charlie + delta + echo + foxtrot + golf + hotel + india + juliet + kilo;";
    let mut harness = Harness::new(|path| {
        commit(path, "f.rs", "fn a() {}\n", "init");
        std::fs::write(path.join("f.rs"), format!("fn a() {{}}\n{long}\n")).unwrap();
    });
    let screen = harness.screen(120, 40);
    // The whole line is visible across rows, and its tail starts after a blank gutter.
    assert!(screen.contains("kilo;"), "{screen}");
    let tail = line_with(&screen, "kilo;");
    assert!(
        !tail.contains("+let"),
        "continuation rows carry no sign: {screen}"
    );
    // Staging selects the source line, not a visual row.
    harness.keys(&["enter", "space"]);
    assert!(
        git(harness.path(), &["diff", "--cached"]).contains(long),
        "the whole wrapped line is staged"
    );
    let unwrapped = configured("[plugins.config.\"bcode.git\".ui.diff]\nwrap_lines = false");
    let mut plain = Harness::with_config(
        |path| {
            commit(path, "f.rs", "fn a() {}\n", "init");
            std::fs::write(path.join("f.rs"), format!("fn a() {{}}\n{long}\n")).unwrap();
        },
        unwrapped,
    );
    let screen = plain.screen(120, 40);
    assert!(!screen.contains("kilo;"), "{screen}");
}

#[test]
fn staging_can_start_in_hunk_mode() {
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "f.txt", "one\ntwo\n", "init");
            std::fs::write(path.join("f.txt"), "ONE\nTWO\n").unwrap();
        },
        configured("[plugins.config.\"bcode.git\".ui.staging]\nhunk_mode = true"),
    );
    harness.keys(&["enter"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[HUNK]"), "{screen}");
    harness.keys(&["space"]);
    assert_eq!(git(harness.path(), &["show", ":f.txt"]), "ONE\nTWO\n");
}

#[test]
fn skip_hook_prefix_and_description_wrapping_apply_on_commit() {
    let mut harness = Harness::with_config(
        |path| {
            changed_repo(path);
            std::fs::create_dir_all(path.join("hooks")).unwrap();
            git(path, &["config", "core.hooksPath", "hooks"]);
            let hook = path.join("hooks/pre-commit");
            std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        },
        configured(
            "[plugins.config.\"bcode.git\".ui.commit]\nskip_hook_prefix = \"WIP\"\nwrap_width = 20",
        ),
    );
    harness.keys(&["c"]);
    harness.paste("WIP: a summary line that stays whole");
    harness.keys(&["tab"]);
    harness.paste("one two three four five six seven\n    indented code stays as it is");
    harness.keys(&["alt+enter"]);
    assert_eq!(
        git(harness.path(), &["log", "-1", "--format=%B"]),
        "WIP: a summary line that stays whole\n\none two three four\nfive six seven\n    indented code stays as it is\n\n"
    );
}

#[test]
fn unbinding_a_panel_key_lets_the_shared_list_binding_through() {
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a", "first");
            commit(path, "b", "b", "second");
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.keybindings."git.list"]
            n = "bcode.git.list.up"
            t = "bcode.git.list.down"
            down = "bcode.git.list.down"
            [plugins.config."bcode.git".ui.keybindings."git.commits"]
            n = "none"
            "#,
        ),
    );
    // "t" stays the commits panel's revert key; move with the arrow instead.
    harness.keys(&["4", "down"]);
    assert_eq!(harness.app.view_cursor(View::Commits), 1);
    harness.keys(&["n"]);
    assert_eq!(harness.app.view_cursor(View::Commits), 0);
    // "n" no longer opens the new-branch prompt in the commits panel.
    let screen = harness.screen(120, 30);
    assert!(!screen.contains("New branch"), "{screen}");
}

#[test]
fn overview_logo_fits_its_width_and_never_wraps() {
    use bmux_tui::geometry::Point;

    let rows = [
        "█                 █             ▀ █",
        "█▀▀█ █▀▀▀ █▀▀█ █▀▀█ █▀▀█   █▀▀▀ █ █▀▀",
        "█  █ █    █  █ █  █ █▀▀▀   █ ▀█ █ █",
        "▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀   ▀▀▀▀ ▀ ▀▀▀",
    ];
    let mut harness = Harness::new(|path| commit(path, "a.txt", "a\n", "Initial commit"));
    harness.keys(&["1"]);
    let indented = |screen: &str, row: &str| screen.matches(&format!("│  {row}")).count();
    let screen = harness.screen(140, 30);
    for row in rows {
        assert_eq!(indented(&screen, row), 1, "{screen}");
    }
    assert!(screen.contains("Last commit"), "{screen}");
    // One cell too narrow for the indent: the letters stay whole, flush left.
    let screen = harness.screen(40, 40);
    for row in rows {
        assert_eq!(screen.matches(&format!("│{row}")).count(), 1, "{screen}");
    }

    // The overview is not interactive: its first row has no cursor highlight.
    let buffer = bcode_git_ui::render_buffer(&mut harness.app, 140, 30);
    let row_of = |needle: &str| {
        (0..30u16)
            .find(|y| buffer.row_symbols(*y).unwrap_or_default().contains(needle))
            .unwrap()
    };
    let (blank, logo) = (row_of("bcode git") + 1, row_of(rows[1]));
    let x = u16::try_from(
        buffer
            .row_symbols(logo)
            .unwrap()
            .chars()
            .count()
            .saturating_sub(3),
    )
    .unwrap();
    assert_eq!(
        buffer.get(Point::new(x, logo)).unwrap().style.bg,
        buffer.get(Point::new(x, blank)).unwrap().style.bg
    );
}

fn panel_repo(path: &Path) {
    commit(path, "a.txt", "a\n", "first commit");
    std::fs::write(path.join("a.txt"), "changed\n").unwrap();
}

/// Column where the main column's border starts on the first screen row.
fn main_column_x(screen: &str) -> usize {
    let row = line_with(screen, "Unstaged changes");
    row.chars()
        .position(|ch| ch == '┌')
        .and_then(|first| {
            row.chars()
                .skip(first + 1)
                .position(|ch| ch == '┌')
                .map(|offset| first + 1 + offset)
        })
        .expect("two panes on the row")
}

#[test]
fn dragging_dividers_resizes_panels_and_is_remembered_per_directory() {
    let mut harness = Harness::new(panel_repo);
    let before = main_column_x(&harness.screen(120, 30));
    harness.drag("panels.root.divider.0", 10, 0);
    let after = main_column_x(&harness.screen(120, 30));
    assert_eq!(after, before + 10);

    // A side divider: files gives three rows to branches.
    let files_height = |screen: &str| {
        let lines: Vec<&str> = screen.lines().collect();
        let top = lines
            .iter()
            .position(|line| line.contains("[2] Files"))
            .unwrap();
        let next = lines
            .iter()
            .position(|line| line.contains("[3] Local"))
            .unwrap();
        next - top
    };
    let screen = harness.screen(120, 30);
    let files = files_height(&screen);
    harness.drag("panels.side.divider.1", 0, -3);
    assert_eq!(files_height(&harness.screen(120, 30)), files - 3);

    // Started again in the same directory: the same layout.
    harness.relaunch();
    let screen = harness.screen(120, 30);
    assert_eq!(main_column_x(&screen), after);
    assert_eq!(files_height(&screen), files - 3);

    // Started from another directory: that directory's own (default) layout.
    let sub = harness.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    harness.relaunch_in(&sub);
    assert_eq!(main_column_x(&harness.screen(120, 30)), before);
}

#[test]
fn title_buttons_collapse_close_and_reopen_windows() {
    let mut harness = Harness::new(panel_repo);
    harness.screen(120, 30);
    harness.click("pane-4.action.collapse");
    let screen = harness.screen(120, 30);
    assert!(
        !screen.contains("first commit  T"),
        "commits list hidden: {screen}"
    );
    assert!(line_with(&screen, "[4] Commits").contains('▸'), "{screen}");

    harness.click("pane-5.action.close");
    let screen = harness.screen(120, 30);
    assert!(!screen.contains("[5] Stash"), "{screen}");

    // Both survive a restart.
    harness.relaunch();
    let screen = harness.screen(120, 30);
    assert!(!screen.contains("[5] Stash"), "{screen}");
    assert!(line_with(&screen, "[4] Commits").contains('▸'), "{screen}");

    // The layout menu reopens the closed window; the expand button restores commits.
    harness.keys(&["ctrl+w", "r"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("Show Stash"), "{screen}");
    harness.keys(&["1"]);
    harness.click("pane-4.action.collapse");
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[5] Stash"), "{screen}");
    assert!(screen.contains("first commit"), "{screen}");
    // The last open window cannot be closed.
    for number in ["2", "3", "4", "5"] {
        harness.click(&format!("pane-{number}.action.close"));
        harness.screen(120, 30);
    }
    harness.click("pane-1.action.close");
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[1] Status"), "{screen}");
}

#[test]
fn double_clicking_a_title_maximizes_and_restores_the_pane() {
    let mut harness = Harness::new(panel_repo);
    harness.screen(120, 30);
    harness.click("pane-2.title");
    harness.click("pane-2.title");
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[2] Files"), "{screen}");
    assert!(!screen.contains("[4] Commits"), "{screen}");
    assert!(!screen.contains("Unstaged changes"), "{screen}");
    harness.click("pane-2.title");
    harness.click("pane-2.title");
    let screen = harness.screen(120, 30);
    assert!(
        screen.contains("[4] Commits") && screen.contains("Unstaged changes"),
        "{screen}"
    );
    // The main view's maximize button.
    harness.click("pane-main-0.action.maximize");
    let screen = harness.screen(120, 30);
    assert!(
        screen.contains("Unstaged changes") && !screen.contains("[2] Files"),
        "{screen}"
    );
}

#[test]
fn keyboard_resizes_and_resets_the_layout() {
    let mut harness = Harness::new(panel_repo);
    let before = main_column_x(&harness.screen(120, 30));
    harness.keys(&["ctrl+w", ">", "ctrl+w", ">"]);
    let wider = main_column_x(&harness.screen(120, 30));
    assert_eq!(wider, before + 4);
    harness.keys(&["ctrl+w", "z"]);
    let screen = harness.screen(120, 30);
    assert!(line_with(&screen, "[2] Files").contains('▸'), "{screen}");
    harness.keys(&["ctrl+w", "="]);
    let screen = harness.screen(120, 30);
    assert_eq!(main_column_x(&screen), before);
    assert!(line_with(&screen, "[2] Files").contains('▾'), "{screen}");
    // Reset also forgets the remembered layout.
    harness.relaunch();
    assert_eq!(main_column_x(&harness.screen(120, 30)), before);
}

#[test]
fn clicking_title_tabs_switches_tabs() {
    let mut harness = Harness::new(panel_repo);
    let screen = harness.screen(140, 30);
    assert!(
        screen.contains("[2] Files · Worktrees · Submodules"),
        "{screen}"
    );
    // A tab of the focused window.
    harness.click("tabs-2.worktrees");
    assert_eq!(harness.app.current_view(), View::Worktrees);
    // A tab of another window focuses that window too.
    harness.screen(140, 30);
    harness.click("tabs-3.tags");
    assert_eq!(harness.app.focused_window(), Window::Side(2));
    assert_eq!(harness.app.current_view(), View::Tags);
    let screen = harness.screen(140, 30);
    assert!(screen.contains("No tags"), "{screen}");
    // Keyboard tab switching still works from there.
    harness.keys(&["["]);
    assert_eq!(harness.app.current_view(), View::Remotes);

    // Clicking a tab of a collapsed window expands it on that tab.
    harness.click("pane-2.action.collapse");
    harness.screen(140, 30);
    harness.click("tabs-2.files");
    let screen = harness.screen(140, 30);
    assert_eq!(harness.app.current_view(), View::Files);
    assert!(line_with(&screen, "[2] Files").contains('▾'), "{screen}");
    assert!(screen.contains(" M a.txt"), "{screen}");
}

#[test]
fn tabs_clipped_from_a_narrow_title_are_not_clickable() {
    let mut harness = Harness::new(panel_repo);
    let has = |harness: &Harness, id: &str| {
        harness
            .app
            .hit_regions()
            .iter()
            .any(|(candidate, _)| candidate == id)
    };
    // At 100 columns only the "S" of "Submodules" fits: it is clickable on that cell only.
    let screen = harness.screen(100, 40);
    assert!(
        line_with(&screen, "[2] Files").contains("Worktrees · S─"),
        "{screen}"
    );
    assert_eq!(harness.region("tabs-2.submodules").width, 1);
    // Narrower, the tab is clipped away entirely and has no region.
    let screen = harness.screen(90, 40);
    assert!(!line_with(&screen, "[2] Files").contains("· S"), "{screen}");
    assert!(has(&harness, "tabs-2.files"));
    assert!(!has(&harness, "tabs-2.submodules"));
}

fn log_shown() -> LoadedConfig {
    configured("[plugins.config.\"bcode.git\".ui.layout.command_log]\nshow = true")
}

#[test]
fn a_closed_command_log_stays_closed_after_reopening() {
    let mut harness = Harness::with_config(panel_repo, log_shown());
    assert!(harness.screen(120, 30).contains("Command log"));
    harness.click("pane-log.action.close");
    assert!(!harness.screen(120, 30).contains("Command log"));
    harness.quit_and_relaunch(log_shown());
    let screen = harness.screen(120, 30);
    assert!(!screen.contains("Command log"), "{screen}");
    // Showing it again is remembered too.
    harness.keys(&["@"]);
    harness.quit_and_relaunch(log_shown());
    assert!(harness.screen(120, 30).contains("Command log"));
}

#[test]
fn reopening_resumes_focus_tabs_selection_and_modes() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "first");
        commit(path, "b", "b", "second");
        commit(path, "c", "c", "third");
        std::fs::create_dir_all(path.join("dir")).unwrap();
        std::fs::write(path.join("dir/x.txt"), "x").unwrap();
    });
    harness.screen(120, 30);
    // Flat files, tags tab, the second commit selected, more diff context, maximized.
    harness.keys(&["2", "`", "3", "]", "]", "4", "j", "}", "ctrl+w", "o"]);
    assert_eq!(harness.app.view_cursor(View::Commits), 1);
    harness.quit_and_relaunch(LoadedConfig::default());
    let screen = harness.screen(120, 30);
    assert_eq!(harness.app.focused_window(), Window::Side(3));
    assert_eq!(harness.app.view_cursor(View::Commits), 1);
    // Maximized: only the commits window shows.
    assert!(
        screen.contains("[4] Commits") && !screen.contains("[2] Files"),
        "{screen}"
    );
    harness.keys(&["ctrl+w", "o", "3"]);
    assert_eq!(harness.app.current_view(), View::Tags);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("dir/x.txt"), "files stay flat: {screen}");
    // The selection follows its commit, not its index, when history changes meanwhile.
    git(
        harness.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "fourth"],
    );
    harness.quit_and_relaunch(LoadedConfig::default());
    harness.screen(120, 30);
    harness.keys(&["4"]);
    let screen = harness.screen(120, 30);
    assert_eq!(harness.app.view_cursor(View::Commits), 2, "{screen}");
}

#[test]
fn reopening_resumes_drilled_in_views_and_line_staging() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "first");
        commit(path, "b.txt", "b\n", "second");
        std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
    });
    harness.screen(120, 30);
    // The files of the latest commit, in the commits window.
    harness.keys(&["4", "enter"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("b.txt"), "{screen}");
    harness.quit_and_relaunch(LoadedConfig::default());
    let screen = harness.screen(120, 30);
    assert!(line_with(&screen, "[4]").contains("Files ·"), "{screen}");
    assert!(screen.contains("b.txt"), "{screen}");

    // Line staging of a changed file.
    harness.keys(&["escape", "2", "enter", "j"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[LINE]"), "{screen}");
    harness.quit_and_relaunch(LoadedConfig::default());
    let screen = harness.screen(120, 30);
    assert!(screen.contains("[LINE]"), "{screen}");
    assert_eq!(harness.app.focused_window(), Window::Main);
}

#[test]
fn changes_are_saved_without_quitting() {
    let mut harness = Harness::with_config(panel_repo, log_shown());
    harness.screen(120, 30);
    harness.click("pane-log.action.close");
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick));
    // Started again without quitting first (for example after a crash).
    let mut config = log_shown();
    config.ui.refresh.watch = false;
    config.ui.refresh.poll_seconds = 0;
    harness.app = launch(harness.path(), config, None, harness.state.path());
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert!(!harness.screen(120, 30).contains("Command log"));
}

#[test]
fn normal_and_half_mode_share_the_side_width() {
    let mut harness = Harness::new(panel_repo);
    let x = |screen: &str| {
        let row = line_with(screen, "┌ Unstaged");
        let byte = row.find("┌ Unstaged").unwrap();
        row[..byte].chars().count()
    };
    let before = x(&harness.screen(120, 30));
    harness.drag("panels.root.divider.0", 10, 0);
    let widened = x(&harness.screen(120, 30));
    assert_eq!(widened, before + 10);
    // Half mode keeps the width dragged in normal mode, and the other way around.
    harness.keys(&["+"]);
    assert_eq!(x(&harness.screen(120, 30)), widened);
    harness.drag("panels.root.divider.0", -4, 0);
    harness.keys(&["_"]);
    assert_eq!(x(&harness.screen(120, 30)), widened - 4);
}

#[test]
fn side_titles_keep_their_padding() {
    let mut clean = Harness::new(|path| commit(path, "a", "a", "init"));
    let screen = clean.screen(140, 30);
    assert!(
        line_with(&screen, "[2] Files").contains("Submodules ─"),
        "{screen}"
    );
    assert!(
        line_with(&screen, "[5] Stash").contains("Stash ─"),
        "{screen}"
    );
}

#[test]
fn untitled_main_views_keep_an_unbroken_border() {
    let mut clean = Harness::new(|path| commit(path, "a", "a", "init"));
    let screen = clean.screen(140, 30);
    let top = screen.lines().next().unwrap();
    assert!(top.contains("┐┌──"), "{top}");
    assert!(!top.contains("┌ "), "{top}");
    assert!(screen.contains("Working tree clean"), "{screen}");
    // Titled views keep their padding.
    let mut dirty = Harness::new(panel_repo);
    let screen = dirty.screen(140, 30);
    assert!(
        screen
            .lines()
            .next()
            .unwrap()
            .contains("┌ Unstaged changes a.txt ─"),
        "{screen}"
    );
}

#[test]
fn shell_prompt_recalls_previous_commands_across_restarts() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    // The input learns its width when drawn, as it is between keystrokes in the terminal.
    let press = |harness: &mut Harness, key: &str| {
        harness.screen(120, 30);
        harness.keys(&[key]);
        harness.screen(120, 30)
    };
    for command in ["echo first", "echo second"] {
        harness.keys(&[":"]);
        harness.paste(command);
        harness.keys(&["enter", "escape"]);
    }
    harness.keys(&[":"]);
    harness.paste("draft");
    assert!(press(&mut harness, "up").contains("│ echo second"));
    assert!(press(&mut harness, "up").contains("│ echo first"));
    assert!(press(&mut harness, "down").contains("│ echo second"));
    let screen = press(&mut harness, "down");
    assert!(screen.contains("│ draft"), "{screen}");
    harness.keys(&["escape"]);

    harness.quit_and_relaunch(LoadedConfig::default());
    harness.keys(&[":"]);
    assert!(press(&mut harness, "up").contains("│ echo second"));
}

#[test]
fn panels_behind_a_popup_scroll_under_the_pointer() {
    use bmux_tui::event::{MouseButton, MouseEventKind};

    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        let long: String = (0..60).map(|line| format!("line {line}\n")).collect();
        std::fs::write(path.join("a.txt"), long).unwrap();
    });
    harness.screen(140, 40);
    harness.keys(&["c"]);
    harness.screen(140, 40);
    let diff = harness.region("main-0");
    // Over the diff, beside the commit editor (its right edge column).
    let x = diff.right() - 2;
    let y = diff.y + 2;
    harness.mouse(MouseEventKind::ScrollDown, x, y);
    harness.mouse(MouseEventKind::ScrollDown, x, y);
    assert_eq!(harness.app.list_scroll("main-0"), 6);
    // The editor stays open and keeps the keyboard; focus behind it is unchanged.
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Summary"), "{screen}");
    assert_eq!(harness.app.focused_window(), Window::Side(1));
    // A side list scrolls without taking focus.
    let commits = harness.region("side-4");
    harness.mouse(MouseEventKind::ScrollDown, commits.x + 1, commits.y);
    assert_eq!(harness.app.focused_window(), Window::Side(1));
    // Over the popup itself, and for clicks behind it, nothing happens.
    let screen = harness.screen(140, 40);
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Summary"))
        .unwrap();
    let column = line[..line.find("Summary").unwrap()].chars().count();
    harness.mouse(
        MouseEventKind::ScrollUp,
        u16::try_from(column).unwrap(),
        u16::try_from(row).unwrap(),
    );
    harness.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
    assert_eq!(harness.app.list_scroll("main-0"), 6);
    assert_eq!(harness.app.focused_window(), Window::Side(1));
    harness.paste("still typing");
    assert!(harness.screen(140, 40).contains("still typing"));
}

#[test]
fn popups_scroll_with_the_mouse_wheel() {
    use bmux_tui::event::{MouseButton, MouseEventKind};

    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&[":"]);
    harness.paste("seq -f 'row-%03g' 1 100");
    harness.keys(&["enter"]);
    let screen = harness.screen(120, 30);
    assert!(screen.contains("row-001"), "{screen}");
    assert!(!screen.contains("row-080"), "{screen}");
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("row-005"))
        .unwrap();
    let x = u16::try_from(line[..line.find("row-005").unwrap()].chars().count()).unwrap();
    let y = u16::try_from(row).unwrap();
    for _ in 0..30 {
        harness.mouse(MouseEventKind::ScrollDown, x, y);
    }
    let screen = harness.screen(120, 30);
    assert!(screen.contains("row-080"), "{screen}");
    assert!(!screen.contains("row-001"), "{screen}");
    // Back up to the top, and no further.
    for _ in 0..40 {
        harness.mouse(MouseEventKind::ScrollUp, x, y);
    }
    assert!(harness.screen(120, 30).contains("row-001"));
    // Its scrollbar, overlaid on the last column, drags to the end.
    let lines = harness.region("popup-text");
    let column = lines.right() - 1;
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, lines.y);
    harness.mouse(
        MouseEventKind::Drag(MouseButton::Left),
        column,
        lines.bottom() + 3,
    );
    harness.mouse(
        MouseEventKind::Up(MouseButton::Left),
        column,
        lines.bottom() + 3,
    );
    let screen = harness.screen(120, 30);
    assert!(screen.contains("row-100"), "{screen}");

    // Menus scroll their items the same way.
    harness.keys(&["escape", "?"]);
    let before = harness.screen(120, 30);
    let (row, line) = before
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Keybindings"))
        .unwrap();
    let x = u16::try_from(line[..line.find("Keybindings").unwrap()].chars().count()).unwrap();
    let y = u16::try_from(row + 3).unwrap();
    for _ in 0..20 {
        harness.mouse(MouseEventKind::ScrollDown, x, y);
    }
    let after = harness.screen(120, 30);
    assert!(after.contains("Keybindings"), "{after}");
    assert_ne!(before, after);
}

/// Background colors of a region's cells, row by row: what a cursor row would change.
fn backgrounds(
    harness: &mut Harness,
    area: bmux_tui::geometry::Rect,
) -> Vec<Vec<Option<bmux_tui::style::Color>>> {
    use bmux_tui::geometry::Point;

    let buffer = bcode_git_ui::render_buffer(&mut harness.app, 140, 40);
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer.get(Point::new(x, y)).unwrap().style.bg)
                .collect()
        })
        .collect()
}

fn long_change(path: &Path) {
    let lines =
        |prefix: &str| -> String { (0..120).map(|line| format!("{prefix} {line}\n")).collect() };
    commit(path, "a.txt", &lines("old"), "init");
    std::fs::write(path.join("a.txt"), lines("new")).unwrap();
}

fn many_commits(path: &Path) {
    for index in 0..60 {
        commit(
            path,
            "a.txt",
            &format!("{index}\n"),
            &format!("commit {index}"),
        );
    }
}

#[test]
fn main_view_reads_as_a_page_until_staging() {
    use bmux_tui::event::{MouseButton, MouseEventKind};

    let mut harness = Harness::new(long_change);
    harness.screen(140, 40);
    let diff = harness.region("main-0");
    let unfocused = backgrounds(&mut harness, diff);
    // Focusing the page shows no cursor row.
    harness.mouse(
        MouseEventKind::Down(MouseButton::Left),
        diff.x + 4,
        diff.y + 3,
    );
    assert_eq!(harness.app.focused_window(), Window::Main);
    assert_eq!(backgrounds(&mut harness, diff), unfocused);
    // Keys scroll it like a page.
    harness.keys(&["j", "j", "j"]);
    assert_eq!(harness.app.list_scroll("main-0"), 3);
    harness.keys(&["k"]);
    assert_eq!(harness.app.list_scroll("main-0"), 2);
    harness.keys(&["pagedown"]);
    assert!(harness.app.list_scroll("main-0") > 20);
    harness.keys(&["home"]);
    assert_eq!(harness.app.list_scroll("main-0"), 0);
    harness.keys(&["end"]);
    let bottom = harness.app.list_scroll("main-0");
    assert!(bottom > 200, "{bottom}");
    harness.screen(140, 40);
    harness.keys(&["home", "escape"]);

    // Staging shows a cursor; the wheel scrolls past it without moving it.
    harness.keys(&["enter"]);
    harness.screen(140, 40);
    assert_ne!(backgrounds(&mut harness, diff), unfocused);
    let selected = harness.app.staging_selection();
    assert!(!selected.is_empty());
    harness.mouse(MouseEventKind::ScrollDown, diff.x + 4, diff.y + 3);
    harness.mouse(MouseEventKind::ScrollDown, diff.x + 4, diff.y + 3);
    assert_eq!(harness.app.list_scroll("main-0"), 6);
    assert_eq!(harness.app.staging_selection(), selected);
    // Moving the cursor brings it back into view.
    harness.keys(&["j"]);
    harness.screen(140, 40);
    assert!(harness.app.list_scroll("main-0") < 6);
}

#[test]
fn hunk_keys_scroll_the_page_to_each_hunk() {
    let mut harness = Harness::new(|path| {
        let text = |changed: &[usize]| -> String {
            (0..200)
                .map(|line| {
                    if changed.contains(&line) {
                        format!("changed {line}\n")
                    } else {
                        format!("line {line}\n")
                    }
                })
                .collect()
        };
        commit(path, "a.txt", &text(&[]), "init");
        // Ten hunks, more than a page.
        let changed: Vec<usize> = (0..10).map(|hunk| hunk * 20 + 10).collect();
        std::fs::write(path.join("a.txt"), text(&changed)).unwrap();
    });
    harness.screen(140, 40);
    harness.click("main-0");
    let first_row = |harness: &mut Harness| {
        let diff = harness.region("main-0");
        harness
            .screen(140, 40)
            .lines()
            .nth(usize::from(diff.y))
            .unwrap()
            .to_owned()
    };
    harness.keys(&["n"]);
    let row = first_row(&mut harness);
    assert!(row.contains("@@"), "{row}");
    let first = harness.app.list_scroll("main-0");
    assert!(first > 0);
    harness.keys(&["n"]);
    assert!(first_row(&mut harness).contains("@@"));
    assert!(harness.app.list_scroll("main-0") > first);
    harness.keys(&["shift+n"]);
    assert_eq!(harness.app.list_scroll("main-0"), first);
}

#[test]
fn side_lists_scroll_without_moving_the_selection() {
    use bmux_tui::event::MouseEventKind;

    let mut harness = Harness::new(many_commits);
    harness.screen(140, 40);
    harness.keys(&["4"]);
    let main = harness.screen(140, 40);
    let commits = harness.region("side-4");
    for _ in 0..5 {
        harness.mouse(MouseEventKind::ScrollDown, commits.x + 2, commits.y + 1);
    }
    assert_eq!(harness.app.list_scroll("side-4"), 15);
    assert_eq!(harness.app.view_cursor(View::Commits), 0);
    // The selection, and so the main view, did not change.
    let scrolled = harness.screen(140, 40);
    let diff = harness.region("main-0");
    let main_rows = |screen: &str| -> Vec<String> {
        screen
            .lines()
            .skip(usize::from(diff.y))
            .take(usize::from(diff.height))
            .map(|line| line.chars().skip(usize::from(diff.x)).collect())
            .collect()
    };
    assert_eq!(main_rows(&scrolled), main_rows(&main));

    // The scroll position survives a restart.
    harness.quit_and_relaunch(LoadedConfig::default());
    harness.screen(140, 40);
    assert_eq!(harness.app.list_scroll("side-4"), 15);

    // Moving the selection brings it back into view.
    harness.keys(&["j"]);
    harness.screen(140, 40);
    assert_eq!(harness.app.view_cursor(View::Commits), 1);
    assert!(harness.app.list_scroll("side-4") <= 1);
}

#[test]
fn border_scrollbars_appear_on_overflow_and_drag() {
    use bmux_tui::event::{MouseButton, MouseEventKind};

    let mut harness = Harness::new(many_commits);
    harness.screen(140, 40);
    let regions: Vec<String> = harness
        .app
        .hit_regions()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    // The commits overflow their pane; the one-line status does not.
    assert!(
        regions.iter().any(|id| id == "pane-4.scrollbar"),
        "{regions:?}"
    );
    assert!(
        !regions.iter().any(|id| id == "pane-1.scrollbar"),
        "{regions:?}"
    );
    let track = harness.region("pane-4.scrollbar");
    let screen = harness.screen(140, 40);
    let column: String = screen
        .lines()
        .skip(usize::from(track.y))
        .take(usize::from(track.height))
        .filter_map(|line| line.chars().nth(usize::from(track.x)))
        .collect();
    assert!(column.starts_with('┃'), "{column}");

    // Dragging the thumb to the bottom scrolls to the end without selecting anything.
    harness.mouse(MouseEventKind::Down(MouseButton::Left), track.x, track.y);
    harness.mouse(
        MouseEventKind::Drag(MouseButton::Left),
        track.x + 10,
        track.bottom() + 5,
    );
    harness.mouse(
        MouseEventKind::Up(MouseButton::Left),
        track.x + 10,
        track.bottom() + 5,
    );
    harness.screen(140, 40);
    assert!(harness.app.list_scroll("side-4") > 0);
    assert_eq!(harness.app.view_cursor(View::Commits), 0);
    let column: String = harness
        .screen(140, 40)
        .lines()
        .skip(usize::from(track.y))
        .take(usize::from(track.height))
        .filter_map(|line| line.chars().nth(usize::from(track.x)))
        .collect();
    assert!(column.ends_with('┃'), "{column}");
}

#[test]
fn command_log_shows_why_a_command_failed() {
    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "init");
        let remote = path.join(".git").join("remote.git");
        git(
            path,
            &["init", "--quiet", "--bare", remote.to_str().unwrap()],
        );
        git(path, &["remote", "add", "origin", remote.to_str().unwrap()]);
        git(path, &["push", "--quiet", "-u", "origin", "main"]);
        git(path, &["branch", "--track", "other", "origin/main"]);
        // `other` is checked out in another worktree, so git refuses to fetch into it.
        let worktree = path.join(".git").join("other-worktree");
        git(
            path,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "other",
            ],
        );
    });
    harness.keys(&["3", "j", "f", "@"]);
    let screen = harness.screen(160, 50);
    let failed = screen
        .lines()
        .position(|line| line.contains("✗ git fetch"))
        .unwrap_or_else(|| panic!("{screen}"));
    let reason = screen.lines().nth(failed + 1).unwrap();
    assert!(reason.contains("refusing to fetch into branch"), "{screen}");
}
