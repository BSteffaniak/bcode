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
    pulls: Option<Arc<dyn bcode_forge_models::PullRequestSource>>,
    host: Arc<dyn bcode_git_ui::host::GitUiHost>,
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
        config: LoadedConfig,
        commit_message: Option<String>,
    ) -> Self {
        Self::with_forge(
            setup,
            config,
            commit_message,
            None,
            Arc::new(bcode_git_ui::host::StandaloneHost),
        )
    }

    fn with_forge(
        setup: impl FnOnce(&Path),
        mut config: LoadedConfig,
        commit_message: Option<String>,
        pulls: Option<Arc<dyn bcode_forge_models::PullRequestSource>>,
        host: Arc<dyn bcode_git_ui::host::GitUiHost>,
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
        let app = launch_with(
            path,
            config,
            commit_message,
            state.path(),
            pulls.clone(),
            host.clone(),
        );
        let mut harness = Self {
            dir,
            state,
            app,
            pulls,
            host,
        };
        harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
        harness
    }

    /// Start the client again from `cwd` (inside the same repository), with the same
    /// remembered state.
    fn relaunch_in(&mut self, cwd: &Path) {
        let mut config = LoadedConfig::default();
        config.ui.refresh.watch = false;
        config.ui.refresh.poll_seconds = 0;
        self.app = launch_with(
            cwd,
            config,
            None,
            self.state.path(),
            self.pulls.clone(),
            self.host.clone(),
        );
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
        self.app = launch_with(
            self.dir.path(),
            config,
            None,
            self.state.path(),
            self.pulls.clone(),
            self.host.clone(),
        );
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

    /// Press a key without applying finished background work, so the screen shows work as it
    /// runs; [`Self::drain`] applies it.
    fn key_held(&mut self, key: &str) {
        let stroke = bmux_keyboard::parse_key_stroke(key).unwrap();
        self.app
            .update(RuntimeEvent::Terminal(Event::Key(stroke)))
            .unwrap();
    }

    fn drain(&mut self) {
        self.app.drain_inline().unwrap();
    }

    fn paste(&mut self, text: &str) {
        self.send(RuntimeEvent::Terminal(Event::Paste(text.to_owned())));
    }

    /// Tick, as the runtime does, until shell commands started from the UI finish (at most
    /// ten seconds), painting in between so views size their terminals.
    fn settle(&mut self, width: u16, height: u16) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick));
            let screen = self.screen(width, height);
            if !self.app.terminal_running() {
                return screen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "still running: {screen}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Tick until `text` shows (at most ten seconds).
    fn wait_for(&mut self, text: &str, width: u16, height: u16) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            self.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick));
            let screen = self.screen(width, height);
            if screen.contains(text) {
                return screen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {text:?}: {screen}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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
    launch_with(
        path,
        config,
        commit_message,
        state,
        None,
        Arc::new(bcode_git_ui::host::StandaloneHost),
    )
}

fn launch_with(
    path: &Path,
    config: LoadedConfig,
    commit_message: Option<String>,
    state: &Path,
    pull_requests: Option<Arc<dyn bcode_forge_models::PullRequestSource>>,
    host: Arc<dyn bcode_git_ui::host::GitUiHost>,
) -> App {
    let mut app = build_app(RunOptions {
        path: path.to_owned(),
        config,
        host,
        commit_message,
        state_dir: Some(state.to_owned()),
        pull_requests,
        daemon: None,
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
    let screen = harness.settle(120, 30);
    assert_eq!(
        std::fs::read_to_string(harness.path().join("a.bak")).unwrap(),
        "b\n"
    );
    // Without `show_output`, its output popup closed when it succeeded.
    assert!(!screen.contains("$ cp"), "{screen}");
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
    // View "other" branch commits (found by filtering, since branches are ordered by commit
    // time), copy the top commit, go back and paste onto main.
    harness.keys(&["3", "/"]);
    harness.paste("other");
    harness.keys(&[
        "enter", "enter", "shift+c", "escape", "4", "shift+v", "enter",
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
fn a_commit_consumes_the_message_file_it_was_prefilled_from() {
    let mut harness = Harness::new(|path| {
        changed_repo(path);
        std::fs::write(path.join(".git/SQUASH_MSG"), "Squashed work").unwrap();
    });
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("from SQUASH_MSG"), "{screen}");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["Squashed work", "init"]);
    assert_eq!(git_file(&harness, "SQUASH_MSG"), None);
}

#[test]
fn a_landed_commits_message_is_not_offered_again_after_head_moves() {
    let mut harness = Harness::new(changed_repo);
    harness.keys(&["c"]);
    harness.paste("Landed");
    harness.keys(&["enter"]);
    assert_eq!(harness.log(), ["Landed", "init"]);
    // `HEAD` moving away would make a leftover COMMIT_EDITMSG look like a failed commit.
    git(harness.path(), &["reset", "--quiet", "--soft", "HEAD~1"]);
    assert_eq!(git_file(&harness, "COMMIT_EDITMSG"), None);
    harness.keys(&["c"]);
    let screen = harness.screen(140, 40);
    assert!(screen.contains("Commit"), "{screen}");
    assert!(!screen.contains("from COMMIT_EDITMSG"), "{screen}");
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
        // Comment lines start with `#` here, whatever the user's global git configuration says.
        git(path, &["config", "core.commentChar", "#"]);
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
    // A finished command's terminal shows the end of its output.
    let screen = harness.settle(120, 30);
    assert!(screen.contains("row-100"), "{screen}");
    assert!(!screen.contains("row-001"), "{screen}");
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("row-095"))
        .unwrap();
    let x = u16::try_from(line[..line.find("row-095").unwrap()].chars().count()).unwrap();
    let y = u16::try_from(row).unwrap();
    for _ in 0..40 {
        harness.mouse(MouseEventKind::ScrollUp, x, y);
    }
    let screen = harness.screen(120, 30);
    assert!(screen.contains("row-001"), "{screen}");
    assert!(!screen.contains("row-100"), "{screen}");
    // Its scrollbar, inside the popup, drags back to the end: grab the thumb.
    let (thumb_y, thumb_line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains('█'))
        .unwrap();
    let column = u16::try_from(thumb_line.chars().position(|ch| ch == '█').unwrap()).unwrap();
    let thumb_y = u16::try_from(thumb_y).unwrap();
    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, thumb_y);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), column, 29);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), column, 29);
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
        // `other` is checked out in another worktree with a commit of its own, and its
        // upstream moves on: it cannot fast-forward.
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
        commit(&worktree, "b", "b", "mine");
        commit(path, "a", "changed", "theirs");
        git(path, &["push", "--quiet", "origin", "main"]);
    });
    // The failure opens git's full message; the command log keeps it after closing.
    harness.keys(&["3", "j", "f"]);
    let screen = harness.screen(160, 50);
    assert!(screen.contains("Not possible to fast-forward"), "{screen}");
    harness.keys(&["escape", "@"]);
    let screen = harness.screen(160, 50);
    let failed = screen
        .lines()
        .position(|line| line.contains("✗ git -C"))
        .unwrap_or_else(|| panic!("{screen}"));
    // Its terminal output follows, ending with git's reason.
    assert!(
        screen
            .lines()
            .skip(failed + 1)
            .take(12)
            .any(|line| line.contains("fatal: Not possible to fast-forward")),
        "{screen}"
    );
}

#[test]
fn shell_commands_show_live_output_and_can_be_interrupted() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&[":"]);
    // The command line (shown in the title) does not contain the words it prints.
    harness.paste("printf '\\033[32mfi%st\\033[0m\\n' rs; sleep 30; echo ne''ver");
    harness.keys(&["enter"]);
    // Output shows while the command still runs.
    let screen = harness.wait_for("first", 120, 30);
    assert!(screen.contains("running"), "{screen}");
    assert!(harness.app.terminal_running());
    // Colors survive: the terminal is real.
    let buffer = bcode_git_ui::render_buffer(&mut harness.app, 120, 30);
    // The output line, not the title showing the command.
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("first"))
        .unwrap();
    let x = line[..line.find("first").unwrap()].chars().count();
    let cell = buffer
        .get(bmux_tui::geometry::Point::new(
            u16::try_from(x).unwrap(),
            u16::try_from(y).unwrap(),
        ))
        .unwrap();
    assert_eq!(cell.style.fg, Some(bmux_tui::style::Color::Green));
    // Interrupting stops it, and the popup stays to show how it ended.
    harness.keys(&["ctrl+c"]);
    let screen = harness.settle(120, 30);
    assert!(!screen.contains("never"), "{screen}");
    assert!(screen.contains("$ printf"), "{screen}");
    assert!(
        screen.contains("stopped") || screen.contains("exit"),
        "{screen}"
    );
    harness.keys(&["escape"]);
    assert!(!harness.screen(120, 30).contains("$ printf"));
}

#[test]
fn failing_shell_commands_keep_their_output_and_exit_status() {
    let mut harness = Harness::new(|path| commit(path, "a", "a", "init"));
    harness.keys(&[":"]);
    harness.paste("echo oops; exit 3");
    harness.keys(&["enter"]);
    let screen = harness.settle(120, 30);
    assert!(screen.contains("oops"), "{screen}");
    assert!(screen.contains("exit 3"), "{screen}");
}

#[cfg(unix)]
#[test]
fn command_log_shows_hook_output_from_a_terminal() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "init");
        git(path, &["config", "core.hooksPath", ".git/hooks"]);
        let hook = path.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\nif [ -t 1 ]; then echo 'checks passed on a terminal'; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(path.join("a"), "changed").unwrap();
    });
    harness.keys(&["c"]);
    harness.paste("change a");
    harness.keys(&["enter", "@"]);
    let screen = harness.screen(160, 50);
    let commit = screen
        .lines()
        .position(|line| line.contains("✓ git commit"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        screen
            .lines()
            .skip(commit + 1)
            .take(12)
            .any(|line| line.contains("checks passed on a terminal")),
        "{screen}"
    );
}

/// Style of the first cell of the first `text` on screen in a line containing `context`.
fn style_at(harness: &mut Harness, text: &str, context: &str) -> bmux_tui::style::Style {
    use bmux_tui::geometry::Point;

    let screen = harness.screen(160, 40);
    let buffer = bcode_git_ui::render_buffer(&mut harness.app, 160, 40);
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(text) && line.contains(context))
        .unwrap_or_else(|| panic!("no {text:?} with {context:?}: {screen}"));
    let x = line[..line.find(text).unwrap()].chars().count();
    buffer
        .get(Point::new(
            u16::try_from(x).unwrap(),
            u16::try_from(y).unwrap(),
        ))
        .unwrap()
        .style
}

const FEATURE_RULE: &str = "[plugins.config.\"bcode.git\".ui.branches]\n\
    colors = [{ pattern = \"^feature/\", color = \"magenta\" }]\n";

#[test]
fn branch_colors_follow_rules_and_match_across_views() {
    use bmux_tui::style::{Color, Modifier};

    let config = configured(&format!("{FEATURE_RULE}dim_merged = false\n"));
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a", "init");
            git(path, &["branch", "feature/x"]);
        },
        config,
    );
    harness.keys(&["3"]);
    let listed = style_at(&mut harness, "feature/x", "init");
    assert_eq!(listed.fg, Some(Color::Magenta));
    let current = style_at(&mut harness, "main", "✱");
    assert!(current.modifiers.contains(Modifier::BOLD));
    assert_ne!(current.fg, listed.fg);
    // The same branch on a commit in the commits list has the same color.
    harness.keys(&["4"]);
    let labelled = style_at(&mut harness, "feature/x", "✱ main");
    assert_eq!(labelled.fg, listed.fg);
    assert_eq!(style_at(&mut harness, "main", "✱ main").fg, current.fg);
}

#[test]
fn merged_branches_are_dimmed_but_never_the_current_one() {
    use bmux_tui::style::Modifier;

    let mut harness = Harness::new(|path| {
        commit(path, "a", "a", "init");
        git(path, &["branch", "old"]);
        git(path, &["checkout", "--quiet", "-b", "open"]);
        commit(path, "a", "b", "work");
        git(path, &["checkout", "--quiet", "main"]);
        commit(path, "a", "c", "more");
    });
    harness.keys(&["3"]);
    assert!(
        style_at(&mut harness, "old", "init")
            .modifiers
            .contains(Modifier::DIM)
    );
    assert!(
        !style_at(&mut harness, "open", "work")
            .modifiers
            .contains(Modifier::DIM)
    );
    assert!(
        !style_at(&mut harness, "main", "✱")
            .modifiers
            .contains(Modifier::DIM)
    );
}

#[test]
fn remote_branches_keep_the_remote_color_for_their_prefix() {
    let config = configured(
        "[plugins.config.\"bcode.git\".ui.branches]\n\
         colors = [{ pattern = \"^feature/\", color = \"#ff8800\" }]\n",
    );
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a", "init");
            let remote = path.join(".git").join("remote.git");
            git(
                path,
                &["init", "--quiet", "--bare", remote.to_str().unwrap()],
            );
            git(path, &["remote", "add", "origin", remote.to_str().unwrap()]);
            git(path, &["push", "--quiet", "origin", "main:feature/x"]);
            git(path, &["fetch", "--quiet", "origin"]);
        },
        config,
    );
    harness.keys(&["4"]);
    let prefix = style_at(&mut harness, "origin/feature/x", "✱ main");
    let name = style_at(&mut harness, "feature/x init", "✱ main");
    assert_eq!(name.fg, Some(bmux_tui::style::Color::Rgb(255, 136, 0)));
    assert_ne!(prefix.fg, name.fg);
}

#[test]
fn branch_help_explains_the_colors() {
    let mut harness = Harness::with_config(
        |path| commit(path, "a", "a", "init"),
        configured(FEATURE_RULE),
    );
    harness.keys(&["3", "?"]);
    let screen = harness.screen(160, 50);
    assert!(screen.contains("Branch colors"), "{screen}");
    assert!(screen.contains("/^feature//"), "{screen}");
    assert!(screen.contains("behind  has commits to pull"), "{screen}");
    assert!(screen.contains("merged into the base branch"), "{screen}");
    // Views without branch names do not show it.
    harness.keys(&["escape", "2", "?"]);
    assert!(!harness.screen(160, 50).contains("Branch colors"));
}

/// A forge that answers with a fixed list (or error), counting requests.
struct FakeForge {
    answer: std::sync::Mutex<
        Result<Vec<bcode_forge_models::PullRequest>, bcode_forge_models::ForgeError>,
    >,
    calls: std::sync::atomic::AtomicUsize,
}

impl FakeForge {
    fn new(
        answer: Result<Vec<bcode_forge_models::PullRequest>, bcode_forge_models::ForgeError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            answer: std::sync::Mutex::new(answer),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl bcode_forge_models::PullRequestSource for FakeForge {
    fn serves(&self, repository: &bcode_forge_models::RepositoryRef) -> bool {
        repository.host == "github.com"
    }

    fn pull_requests(
        &self,
        _: &bcode_forge_models::RepositoryRef,
    ) -> Result<bcode_forge_models::PullRequestList, bcode_forge_models::ForgeError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answer.lock().unwrap().clone().map(|pull_requests| {
            bcode_forge_models::PullRequestList {
                pull_requests,
                via: bcode_forge_models::Transport::Http,
            }
        })
    }
}

/// A host that records the pages it is asked to open.
#[derive(Default)]
struct RecordingHost {
    opened: std::sync::Mutex<Vec<String>>,
}

impl bcode_git_ui::host::GitUiHost for RecordingHost {
    fn open_url(&self, url: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(url.to_owned());
        Ok(())
    }
}

fn pull(
    number: u64,
    head: &str,
    state: bcode_forge_models::PullRequestState,
    head_oid: &str,
) -> bcode_forge_models::PullRequest {
    bcode_forge_models::PullRequest {
        number,
        title: format!("Change {number}"),
        url: format!("https://github.com/octo/hello/pull/{number}"),
        state,
        head: head.into(),
        head_repository: Some("octo/hello".into()),
        head_oid: head_oid.into(),
        base: "main".into(),
        review: None,
        checks: None,
        updated_at: "2026-11-05T00:00:00Z".into(),
    }
}

/// A repository on github.com with: `feature/login` (an open, approved, passing pull
/// request), `squashed` (merged by squash: its tip is not in main), and `continued` (merged,
/// then more local work). Returns the pull requests the forge reports.
fn forge_repo(path: &Path) -> Vec<bcode_forge_models::PullRequest> {
    use bcode_forge_models::{ChecksState, PullRequestState, ReviewDecision};

    commit(path, "a", "a", "init");
    git(
        path,
        &["remote", "add", "origin", "git@github.com:octo/hello.git"],
    );
    let branch = |name: &str, file: &str| {
        git(path, &["checkout", "--quiet", "-b", name, "main"]);
        commit(path, file, name, name);
        git(path, &["rev-parse", "HEAD"]).trim().to_owned()
    };
    branch("feature/login", "login");
    let squashed = branch("squashed", "squash");
    let merged = branch("continued", "continued");
    commit(path, "continued", "more", "after the merge");
    git(path, &["checkout", "--quiet", "main"]);
    let mut login = pull(12, "feature/login", PullRequestState::Open, "x");
    login.review = Some(ReviewDecision::Approved);
    login.checks = Some(ChecksState::Passing);
    vec![
        login,
        pull(9, "squashed", PullRequestState::Merged, &squashed),
        pull(8, "continued", PullRequestState::Merged, &merged),
    ]
}

#[test]
fn branches_show_their_pull_requests() {
    use bmux_tui::style::{Color, Modifier};

    let forge = FakeForge::new(Ok(Vec::new()));
    let host = Arc::new(RecordingHost::default());
    let mut harness = Harness::with_forge(
        |path| *forge.answer.lock().unwrap() = Ok(forge_repo(path)),
        configured(
            "[plugins.config.\"bcode.git\".ui.branches]\n\
             colors = [{ pr = \"open\", review = \"approved\", color = \"#123456\" }]\n",
        ),
        None,
        Some(forge.clone()),
        host.clone(),
    );
    harness.keys(&["3"]);
    let screen = harness.screen(160, 40);
    assert!(forge.calls() >= 1);
    assert!(
        line_with(&screen, "feature/login").contains("#12 open ✓ ✔"),
        "{screen}"
    );
    assert!(
        line_with(&screen, "squashed").contains("#9 merged"),
        "{screen}"
    );
    // Rules can test the pull request; merged-by-squash dims, newer local work does not.
    assert_eq!(
        style_at(&mut harness, "feature/login", "#12").fg,
        Some(Color::Rgb(0x12, 0x34, 0x56))
    );
    assert!(
        style_at(&mut harness, "squashed", "#9")
            .modifiers
            .contains(Modifier::DIM)
    );
    assert!(
        !style_at(&mut harness, "continued", "#8")
            .modifiers
            .contains(Modifier::DIM)
    );
    // `o` opens the selected branch's pull request.
    let row = screen
        .lines()
        .position(|line| line.contains("feature/login"))
        .unwrap();
    // Branches start on the row after the window's title.
    let first = screen
        .lines()
        .position(|line| line.contains("[3] Local"))
        .unwrap()
        + 1;
    harness.keys(&["<"]);
    for _ in first..row {
        harness.keys(&["j"]);
    }
    harness.keys(&["o"]);
    assert_eq!(
        host.opened.lock().unwrap().as_slice(),
        ["https://github.com/octo/hello/pull/12"]
    );
    // Its log in the main view starts with the pull request.
    let screen = harness.screen(160, 40);
    assert!(
        screen.contains("Pull request #12 Change 12 · open · approved · checks passing"),
        "{screen}"
    );
    // The legend explains the markers.
    harness.keys(&["?"]);
    assert!(
        harness
            .screen(160, 50)
            .contains("checks passing, failing, running")
    );
}

#[test]
fn pull_requests_are_cached_and_failures_are_explained() {
    let forge = FakeForge::new(Ok(Vec::new()));
    let mut harness = Harness::with_forge(
        |path| *forge.answer.lock().unwrap() = Ok(forge_repo(path)),
        LoadedConfig::default(),
        None,
        Some(forge.clone()),
        Arc::new(bcode_git_ui::host::StandaloneHost),
    );
    // A session started while the cached answer is fresh (as any client's would be) takes
    // it instead of asking again.
    let calls = forge.calls();
    harness.relaunch();
    assert_eq!(forge.calls(), calls);
    // Once it is stale, the next session asks. It cannot reach GitHub, but shows what was
    // cached and why it is stale.
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for entry in std::fs::read_dir(harness.state.path().join("pull-requests")).unwrap() {
        let file = std::fs::File::options()
            .write(true)
            .open(entry.unwrap().path())
            .unwrap();
        file.set_modified(stale).unwrap();
    }
    harness.pulls = Some(FakeForge::new(Err(
        bcode_forge_models::ForgeError::Unauthenticated("no GitHub token for github.com".into()),
    )));
    harness.relaunch();
    harness.keys(&["3"]);
    assert!(line_with(&harness.screen(160, 40), "feature/login").contains("#12 open"));
    harness.keys(&["1"]);
    let screen = harness.screen(160, 40);
    assert!(screen.contains("Pull request"), "{screen}");
    assert!(
        screen.contains("unavailable: no GitHub token for github.com"),
        "{screen}"
    );
}

#[test]
fn other_forges_and_disabled_pull_requests_ask_nothing() {
    let forge = FakeForge::new(Ok(Vec::new()));
    let mut harness = Harness::with_forge(
        |path| {
            commit(path, "a", "a", "init");
            git(
                path,
                &["remote", "add", "origin", "git@gitlab.com:octo/hello.git"],
            );
        },
        LoadedConfig::default(),
        None,
        Some(forge.clone()),
        Arc::new(bcode_git_ui::host::StandaloneHost),
    );
    harness.keys(&["1"]);
    let screen = harness.screen(160, 40);
    assert_eq!(forge.calls(), 0);
    assert!(screen.contains("gitlab.com is not GitHub"), "{screen}");

    let forge = FakeForge::new(Ok(Vec::new()));
    let mut harness = Harness::with_forge(
        forge_repo_ignoring,
        configured("[plugins.config.\"bcode.git\".ui.pull_requests]\nenabled = false\n"),
        None,
        Some(forge.clone()),
        Arc::new(bcode_git_ui::host::StandaloneHost),
    );
    harness.keys(&["1"]);
    let screen = harness.screen(160, 40);
    assert_eq!(forge.calls(), 0);
    assert!(!screen.contains("Pull request"), "{screen}");
}

fn forge_repo_ignoring(path: &Path) {
    forge_repo(path);
}

/// A repository whose `main` tracks `origin/main` in a bare remote and is one commit ahead.
fn ahead_of_origin(path: &Path, remote: &Path) {
    git(
        remote,
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    commit(path, "a", "a", "init");
    git(
        path,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(path, &["push", "--quiet", "-u", "origin", "main"]);
    commit(path, "b", "b", "second");
}

/// The local branch row of `main` (the side pane's part of its line).
fn main_row(screen: &str) -> String {
    screen
        .lines()
        .map(|line| line.chars().take(52).collect::<String>())
        .find(|left| left.contains("✱ main") && !left.contains('●') && !left.contains('→'))
        .unwrap_or_default()
}

fn status_bar(screen: &str) -> &str {
    screen.lines().last().unwrap_or_default()
}

#[test]
fn a_push_shows_on_its_branch_and_in_the_status_bar_while_it_runs() {
    let remote = tempfile::tempdir().unwrap();
    let mut harness = Harness::with_config(
        |path| ahead_of_origin(path, remote.path()),
        configured(
            "[plugins.config.\"bcode.git\".ui.activity]\nanimate = false\nresult_ms = 1000\n",
        ),
    );
    harness.keys(&["3"]);
    assert!(
        main_row(&harness.screen(160, 40)).contains("↑1"),
        "{}",
        harness.screen(160, 40)
    );
    harness.key_held("shift+p");
    let screen = harness.screen(160, 40);
    // Network work shows at once, with git's own progress.
    let row = main_row(&screen);
    assert!(row.contains("… pushing"), "{screen}");
    assert!(row.contains('%'), "{screen}");
    let bar = status_bar(&screen);
    assert!(bar.contains("… Pushing main · Writing objects"), "{screen}");
    // The remote and its branch show it too.
    harness.drain();
    let screen = harness.screen(160, 40);
    let row = main_row(&screen);
    assert!(row.contains("✓ pushed"), "{screen}");
    assert!(!row.contains("↑1"), "{screen}");
    assert!(!status_bar(&screen).contains("Pushing"), "{screen}");
    // The result mark goes after a moment.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick));
    let screen = harness.screen(160, 40);
    assert!(!main_row(&screen).contains("pushed"), "{screen}");
}

#[test]
fn a_failed_push_marks_its_branch() {
    let remote = tempfile::tempdir().unwrap();
    let mut harness = Harness::new(|path| {
        ahead_of_origin(path, remote.path());
        // A global `core.hooksPath` would bypass the remote's own hooks.
        git(remote.path(), &["config", "core.hooksPath", "hooks"]);
        let hook = remote.path().join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho no pushes today >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    });
    harness.keys(&["3", "shift+p"]);
    // Close the error popup to see the row behind it.
    harness.keys(&["escape"]);
    let screen = harness.screen(160, 40);
    assert!(main_row(&screen).contains("✗ failed"), "{screen}");
}

#[test]
fn quick_local_work_never_flashes_a_spinner() {
    let mut harness = Harness::new(|path| {
        commit(path, "a.txt", "a\n", "init");
        std::fs::write(path.join("a.txt"), "changed\n").unwrap();
    });
    harness.keys(&["2"]);
    harness.key_held("space");
    let screen = harness.screen(120, 30);
    assert!(
        !screen
            .chars()
            .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
        "{screen}"
    );
    assert!(!status_bar(&screen).contains("Staged"), "{screen}");
    harness.drain();
}

#[test]
fn automatic_fetches_show_only_in_the_status_bar() {
    let remote = tempfile::tempdir().unwrap();
    let mut harness = Harness::with_config(
        |path| ahead_of_origin(path, remote.path()),
        configured(
            "[plugins.config.\"bcode.git\".ui.refresh]\nauto_fetch_seconds = 1\n\
             [plugins.config.\"bcode.git\".ui.activity]\nanimate = false\n",
        ),
    );
    harness.keys(&["3"]);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    harness
        .app
        .update(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick))
        .unwrap();
    let screen = harness.screen(160, 40);
    assert!(status_bar(&screen).contains("… Fetching"), "{screen}");
    assert!(!main_row(&screen).contains("fetching"), "{screen}");
    harness.drain();
    let screen = harness.screen(160, 40);
    assert!(!main_row(&screen).contains("fetched"), "{screen}");
    assert!(!status_bar(&screen).contains("Fetching"), "{screen}");
}

#[test]
fn automatic_fetches_count_fetches_made_elsewhere() {
    let remote = tempfile::tempdir().unwrap();
    let mut harness = Harness::with_config(
        |path| ahead_of_origin(path, remote.path()),
        configured(
            "[plugins.config.\"bcode.git\".ui.refresh]\nauto_fetch_seconds = 1\n\
             [plugins.config.\"bcode.git\".ui.activity]\nanimate = false\n",
        ),
    );
    let tick = |harness: &mut Harness| {
        harness
            .app
            .update(RuntimeEvent::Message(bcode_git_ui::app::Msg::Tick))
            .unwrap();
        status_bar(&harness.screen(160, 40)).contains("… Fetching")
    };
    // Another client (or a terminal) fetched just now: this one's fetch waits its turn.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    git(harness.path(), &["fetch", "--quiet"]);
    assert!(!tick(&mut harness));
    harness.drain();
    // A full interval after that fetch, this client fetches.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(tick(&mut harness));
    harness.drain();
}

#[test]
fn clicking_running_work_opens_the_command_log() {
    let remote = tempfile::tempdir().unwrap();
    let mut harness = Harness::with_config(
        |path| ahead_of_origin(path, remote.path()),
        configured("[plugins.config.\"bcode.git\".ui.layout.command_log]\nshow = false\n"),
    );
    harness.keys(&["3"]);
    assert!(!harness.screen(160, 40).contains("Command log"));
    harness.key_held("shift+p");
    harness.screen(160, 40);
    harness.click("status-activity");
    harness.drain();
    let screen = harness.screen(160, 40);
    assert!(screen.contains("Command log"), "{screen}");
    assert!(screen.contains("git push"), "{screen}");
}

#[test]
fn views_show_what_is_loading_into_them() {
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a", "a", "init");
            git(path, &["branch", "topic"]);
        },
        configured(
            "[plugins.config.\"bcode.git\".ui.activity]\nanimate = false\nshow_delay_ms = 0\n",
        ),
    );
    harness.keys(&["3"]);
    let before = harness.screen(160, 40);
    assert!(!before.contains('…'), "{before}");
    // Opening a branch loads its commits into the side window and its log into the main view.
    harness.key_held("enter");
    let screen = harness.screen(160, 40);
    let titled = screen
        .lines()
        .filter(|line| line.contains('┌') && line.contains('…'))
        .count();
    assert!(titled >= 1, "{screen}");
    harness.drain();
    let screen = harness.screen(160, 40);
    assert!(!screen.contains('…'), "{screen}");
}

/// The files panel row (the text between its side borders) containing `needle`.
fn files_row(screen: &str, needle: &str) -> String {
    screen
        .lines()
        .filter_map(|line| line.split('│').nth(1))
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no files row contains {needle:?}:\n{screen}"))
        .to_owned()
}

/// The bottom border of the panel titled `title`: the first one after its title row.
fn footer(screen: &str, title: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let title = lines
        .iter()
        .position(|line| line.contains(title))
        .unwrap_or_else(|| panic!("no {title:?} panel:\n{screen}"));
    let footer = lines[title..]
        .iter()
        .find(|line| line.starts_with('└'))
        .unwrap_or_else(|| panic!("no panel border:\n{screen}"));
    // Only the side panel's part of the row.
    footer.split('┘').next().unwrap_or_default().to_owned()
}

#[test]
fn line_counts_show_on_files_folders_and_the_footer() {
    let mut harness = Harness::new(|path| {
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("src/lib.rs"), "x\ny\n").unwrap();
        commit(path, "a.txt", "1\n2\n3\n", "init");
        std::fs::write(path.join("a.txt"), "1\nTWO\n3\n4\n").unwrap();
        std::fs::write(path.join("src/lib.rs"), "x\n").unwrap();
        std::fs::write(path.join("src/new.rs"), "a\nb\n").unwrap();
        std::fs::write(path.join("logo.bin"), b"\x89PNG\0\x01").unwrap();
    });
    let screen = harness.screen(140, 40);
    assert!(
        files_row(&screen, "a.txt").contains("a.txt +2 -1"),
        "{screen}"
    );
    assert!(
        files_row(&screen, "lib.rs").contains("lib.rs -1"),
        "{screen}"
    );
    // An untracked file counts as added; a binary one is marked.
    assert!(
        files_row(&screen, "new.rs").contains("new.rs +2"),
        "{screen}"
    );
    assert!(
        files_row(&screen, "logo.bin").contains("logo.bin bin"),
        "{screen}"
    );
    // A folder sums everything beneath it, and the title sums every file.
    assert!(
        files_row(&screen, "src/").contains("src/ +2 -1"),
        "{screen}"
    );
    assert!(footer(&screen, "[2] Files").contains(" +4 -2 "), "{screen}");

    // A collapsed folder keeps its total.
    harness.keys(&["enter"]);
    let screen = harness.screen(140, 40);
    assert!(!screen.contains("new.rs"), "{screen}");
    assert!(
        files_row(&screen, "src/").contains("src/ +2 -1"),
        "{screen}"
    );
}

#[test]
fn split_line_counts_and_filtered_totals() {
    let mut harness = Harness::with_config(
        |path| {
            std::fs::write(path.join("b.txt"), "b\n").unwrap();
            commit(path, "a.txt", "1\n2\n", "init");
            std::fs::write(path.join("a.txt"), "1\nX\n").unwrap();
            std::fs::write(path.join("b.txt"), "c\n").unwrap();
            git(path, &["add", "--all"]);
            std::fs::write(path.join("a.txt"), "1\nX\n3\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.files.line_counts]
            show = "split"
            "#,
        ),
    );
    let screen = harness.screen(140, 40);
    assert!(
        files_row(&screen, "a.txt").contains("a.txt +1 -1 · +1"),
        "{screen}"
    );
    assert!(
        files_row(&screen, "b.txt").contains("b.txt +1 -1 ·"),
        "{screen}"
    );
    assert!(
        footer(&screen, "[2] Files").contains(" +2 -2 · +1 "),
        "{screen}"
    );

    // Filtered, the title counts only the files left.
    harness.keys(&["/"]);
    harness.paste("b.txt");
    harness.keys(&["enter"]);
    let screen = harness.screen(140, 40);
    let footer = footer(&screen, "[2] Files");
    assert!(footer.contains(" +1 -1 · "), "{screen}");
    assert!(!footer.contains("+2"), "{screen}");
}

#[test]
fn line_counts_can_be_turned_off() {
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a.txt", "1\n", "init");
            std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.files.line_counts]
            show = "off"
            "#,
        ),
    );
    let screen = harness.screen(140, 40);
    assert!(!files_row(&screen, "a.txt").contains('+'), "{screen}");
    assert!(!footer(&screen, "[2] Files").contains('+'), "{screen}");
}

#[test]
fn line_count_position_follows_alignment_and_floating() {
    const LONG: &str = "a/very/long/directory/path/to/the/changed/file.rs";
    let setup = |path: &Path| {
        commit(path, "a.txt", "1\n", "init");
        std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
        std::fs::create_dir_all(path.join("a/very/long/directory/path/to/the/changed")).unwrap();
        std::fs::write(path.join(LONG), "x\n").unwrap();
        std::fs::write(
            path.join("日本語🙂e\u{301}ファイル名とても長い名前.txt"),
            "y\n",
        )
        .unwrap();
    };
    let rows = |align: &str, floating: bool| {
        let mut harness = Harness::with_config(
            setup,
            configured(&format!(
                r#"
                [plugins.config."bcode.git".ui.files]
                tree = false
                [plugins.config."bcode.git".ui.files.line_counts]
                align = "{align}"
                floating = {floating}
                "#
            )),
        );
        let screen = harness.screen(90, 30);
        (
            files_row(&screen, "a.txt"),
            files_row(&screen, "a/very"),
            files_row(&screen, "日本語"),
            screen,
        )
    };

    // After the name: long names push the counts out of view.
    let (short, long, wide, screen) = rows("after-name", false);
    assert!(short.contains("a.txt +1 "), "{screen}");
    assert!(!long.contains("+1"), "{screen}");
    assert!(!wide.contains("+1"), "{screen}");

    // Floating after the name: long names are shortened to keep them visible.
    let (short, long, wide, screen) = rows("after-name", true);
    assert!(short.contains("a.txt +1 "), "{screen}");
    assert!(long.contains("… +1"), "{screen}");
    assert!(wide.contains("… +1"), "{screen}");

    // Right-aligned: against the edge when there is room, else clipped like the name.
    let (short, long, _, screen) = rows("right", false);
    assert!(short.trim_end().ends_with("   +1"), "{screen}");
    assert!(!long.contains("+1"), "{screen}");

    // Floating right: always against the edge.
    let (short, long, wide, screen) = rows("right", true);
    for row in [&short, &long, &wide] {
        assert!(row.trim_end().ends_with(" +1"), "{screen}");
    }
    assert!(long.contains('…') && wide.contains('…'), "{screen}");
    // Every row ends at the same column, wide characters included.
    let width = |row: &str| bmux_tui::text_width::display_width(row.trim_end());
    assert_eq!(width(&short), width(&long), "{screen}");
    assert_eq!(width(&short), width(&wide), "{screen}");
}

#[test]
fn commit_files_show_line_counts() {
    let mut harness = Harness::new(|path| {
        commit(path, "alpha.rs", "a\n", "add alpha");
        std::fs::write(path.join("alpha.rs"), "A\n").unwrap();
        commit(path, "beta.rs", "b\nc\n", "add beta");
    });
    harness.keys(&["4", "enter"]);
    let screen = harness.wait_for("Files ·", 140, 40);
    assert!(
        files_row(&screen, "beta.rs").contains("beta.rs +2"),
        "{screen}"
    );
    assert!(
        files_row(&screen, "alpha.rs").contains("alpha.rs +1 -1"),
        "{screen}"
    );
    assert!(
        footer(&screen, "[4] Files ·").contains(" +3 -1 "),
        "{screen}"
    );
}

/// Backgrounds of the side panel row containing `needle`, between the panel's borders.
fn side_row_backgrounds(
    harness: &mut Harness,
    needle: &str,
) -> Vec<Option<bmux_tui::style::Color>> {
    use bmux_tui::geometry::Point;

    let buffer = bcode_git_ui::render_buffer(&mut harness.app, 140, 40);
    for y in 0..40 {
        let cells: Vec<_> = (0..140)
            .map(|x| buffer.get(Point::new(x, y)).unwrap())
            .collect();
        let Some(left) = cells.iter().position(|cell| cell.symbol == "│") else {
            continue;
        };
        let Some(width) = cells[left + 1..].iter().position(|cell| cell.symbol == "│") else {
            continue;
        };
        let row = &cells[left + 1..left + 1 + width];
        let text: String = row.iter().map(|cell| cell.symbol.as_str()).collect();
        if text.contains(needle) {
            return row.iter().map(|cell| cell.style.bg).collect();
        }
    }
    panic!(
        "no side row contains {needle:?}:\n{}",
        harness.screen(140, 40)
    );
}

/// The one background a row is painted with, which must fill it.
fn row_background(harness: &mut Harness, needle: &str) -> Option<bmux_tui::style::Color> {
    let row = side_row_backgrounds(harness, needle);
    assert!(
        row.iter().all(|background| *background == row[0]),
        "{needle:?} is not one background across: {row:?}"
    );
    row[0]
}

fn flash_kind(background: Option<bmux_tui::style::Color>) -> &'static str {
    match background {
        Some(bmux_tui::style::Color::Rgb(red, green, _)) if green > red => "green",
        Some(bmux_tui::style::Color::Rgb(red, green, _)) if red > green => "red",
        _ => "none",
    }
}

#[test]
fn changed_files_and_folders_flash_green_or_red() {
    let mut harness = Harness::with_config(
        |path| {
            std::fs::create_dir_all(path.join("src")).unwrap();
            std::fs::write(path.join("src/lib.rs"), "1\n2\n3\n").unwrap();
            commit(path, "other.txt", "a\n", "init");
            std::fs::write(path.join("src/lib.rs"), "1\n2\n3\n4\n").unwrap();
            std::fs::write(path.join("other.txt"), "b\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.flash]
            duration_ms = 600000
            "#,
        ),
    );
    // Nothing flashes on the first load.
    let base = row_background(&mut harness, "other.txt");
    assert_eq!(row_background(&mut harness, "lib.rs"), base);
    assert_eq!(flash_kind(base), "none");

    // Added lines flash the file and the folder above it green, across the whole row.
    std::fs::write(harness.path().join("src/lib.rs"), "1\n2\n3\n4\n5\n6\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(flash_kind(row_background(&mut harness, "lib.rs")), "green");
    assert_eq!(flash_kind(row_background(&mut harness, "src/")), "green");
    assert_eq!(row_background(&mut harness, "other.txt"), base);

    // Taking lines away (typed or original) flashes red; a new file flashes green.
    std::fs::write(harness.path().join("src/lib.rs"), "1\n").unwrap();
    std::fs::write(harness.path().join("new.txt"), "").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(flash_kind(row_background(&mut harness, "lib.rs")), "red");
    assert_eq!(flash_kind(row_background(&mut harness, "new.txt")), "green");

    // Committing moves HEAD: rows that remain start over without flashing.
    git(harness.path(), &["add", "src"]);
    git(harness.path(), &["commit", "--quiet", "-m", "lib"]);
    std::fs::write(harness.path().join("other.txt"), "b\nc\nd\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(row_background(&mut harness, "other.txt"), base);
}

#[test]
fn flashes_end_and_can_be_turned_off() {
    let setup = |path: &Path| {
        commit(path, "a.txt", "1\n", "init");
        commit(path, "b.txt", "1\n", "second");
        std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
        std::fs::write(path.join("b.txt"), "1\n2\n").unwrap();
    };
    let mut harness = Harness::with_config(
        setup,
        configured(
            r#"
            [plugins.config."bcode.git".ui.flash]
            duration_ms = 40
            "#,
        ),
    );
    let base = row_background(&mut harness, "b.txt");
    std::fs::write(harness.path().join("b.txt"), "1\n2\n3\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    std::thread::sleep(std::time::Duration::from_millis(60));
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Frame));
    assert_eq!(row_background(&mut harness, "b.txt"), base);

    let mut harness = Harness::with_config(
        setup,
        configured(
            r#"
            [plugins.config."bcode.git".ui.flash]
            enabled = false
            duration_ms = 600000
            "#,
        ),
    );
    let base = row_background(&mut harness, "b.txt");
    std::fs::write(harness.path().join("b.txt"), "1\n2\n3\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(row_background(&mut harness, "b.txt"), base);
}

#[test]
fn branches_flash_and_count_the_changes_in_their_worktrees() {
    let other = TempDir::new().unwrap();
    let worktree = other.path().join("topic");
    let mut harness = Harness::with_config(
        |path| {
            commit(path, "a.txt", "1\n", "init");
            git(
                path,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "topic",
                    worktree.to_str().unwrap(),
                ],
            );
            std::fs::write(worktree.join("a.txt"), "1\n2\n").unwrap();
            std::fs::write(path.join("a.txt"), "1\n2\n3\n").unwrap();
        },
        configured(
            r#"
            [plugins.config."bcode.git".ui.flash]
            duration_ms = 600000
            [plugins.config."bcode.git".ui.branches]
            line_counts = true
            [plugins.config."bcode.git".ui.layout]
            windows = [["status"], ["files"], ["branches"], ["worktrees"], ["commits"]]
            "#,
        ),
    );
    // Each branch counts the changes where it is checked out, once measured.
    let screen = harness.wait_for("topic (worktree)  init +1", 140, 40);
    assert!(line_with(&screen, "✱ main").contains("init +2"), "{screen}");
    assert_eq!(
        flash_kind(row_background(&mut harness, "topic (worktree)")),
        "none"
    );

    // A change here flashes the current branch and its worktree.
    std::fs::write(harness.path().join("a.txt"), "1\n2\n3\n4\n").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(
        flash_kind(row_background(&mut harness, "main  init +3")),
        "green"
    );
    assert_eq!(flash_kind(row_background(&mut harness, "● main")), "green");
    assert_eq!(
        flash_kind(row_background(&mut harness, "topic (worktree)")),
        "none"
    );

    // A change in the other worktree flashes its branch red. (Inline, nothing watches, so
    // the harness reports the change itself.)
    std::fs::write(worktree.join("a.txt"), "").unwrap();
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    harness.wait_for("topic (worktree)  init -1", 140, 40);
    assert_eq!(
        flash_kind(row_background(&mut harness, "topic (worktree)")),
        "red"
    );

    // A new branch flashes green.
    git(harness.path(), &["branch", "fresh"]);
    harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
    assert_eq!(flash_kind(row_background(&mut harness, "fresh")), "green");
}
