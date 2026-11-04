//! End-to-end behavior: keystrokes drive real git operations in temporary repositories.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bcode_git_ui::app::App;
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
    app: App,
}

impl Harness {
    fn new(setup: impl FnOnce(&Path)) -> Self {
        Self::with_config(setup, LoadedConfig::default())
    }

    fn with_config(setup: impl FnOnce(&Path), mut config: LoadedConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "--quiet", "--initial-branch", "main"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        setup(path);
        config.ui.refresh.watch = false;
        config.ui.refresh.poll_seconds = 0;
        let mut app = build_app(RunOptions {
            path: path.to_owned(),
            config,
            host: Arc::new(bcode_git_ui::host::StandaloneHost),
        })
        .unwrap();
        app.set_inline(true);
        let mut harness = Self { dir, app };
        harness.send(RuntimeEvent::Message(bcode_git_ui::app::Msg::Watch));
        harness
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
fn mouse_clicks_focus_and_select_rows_and_wheel_scrolls() {
    use bcode_git_ui::app::{View, Window};
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
    assert_eq!(harness.app.focused_window(), Window::Commits);
    assert_eq!(harness.app.view_cursor(View::Commits), 2);
    harness.screen(140, 40);
    harness.send(click(MouseEventKind::ScrollUp));
    assert_eq!(harness.app.view_cursor(View::Commits), 0);
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
