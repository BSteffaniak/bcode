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
        let mut app = build_app(RunOptions {
            path: path.to_owned(),
            config,
            host: Arc::new(bcode_git_ui::host::StandaloneHost),
            commit_message,
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
fn mouse_clicks_focus_and_select_rows_and_wheel_scrolls() {
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
        .find(|(id, _)| id.starts_with("side-4."))
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
