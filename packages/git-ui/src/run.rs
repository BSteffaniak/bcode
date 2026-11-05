//! Terminal ownership, the runtime loop, and handing the terminal to external programs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bcode_git::{CommandLog, Repository};
use bmux_tui::component::{Constraints, LayoutCx};
use bmux_tui::crossterm::{CrosstermTerminalGuard, terminal_size};
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::{Runtime, RuntimeConfig, TerminalPresenter};

use crate::app::{App, Msg};
use crate::config::LoadedConfig;
use crate::host::GitUiHost;
use crate::keymap::Keymap;
use crate::theme::GitTheme;
use crate::view::{Frame, tree};

/// Inputs for [`run`].
pub struct RunOptions {
    /// Directory inside the repository.
    pub path: PathBuf,
    /// Loaded configuration.
    pub config: LoadedConfig,
    /// Host capabilities.
    pub host: Arc<dyn GitUiHost>,
    /// Message for the first commit editor of the session; the binaries pass
    /// `$BCODE_GIT_COMMIT_MSG` (see [`commit_message_from_env`]).
    pub commit_message: Option<String>,
    /// Where panel layouts are remembered; `None` uses the default state directory.
    pub state_dir: Option<PathBuf>,
}

/// Environment variable holding a commit message to prefill.
pub const COMMIT_MESSAGE_ENV: &str = "BCODE_GIT_COMMIT_MSG";

/// The `BCODE_GIT_COMMIT_MSG` environment variable, when set and not blank.
#[must_use]
pub fn commit_message_from_env() -> Option<String> {
    std::env::var(COMMIT_MESSAGE_ENV)
        .ok()
        .filter(|message| !message.trim().is_empty())
}

/// Failure starting or running the UI.
#[derive(Debug)]
pub enum RunError {
    /// The path is not a repository.
    Repository(String),
    /// Invalid keybinding configuration.
    Keymap(String),
    /// Terminal or runtime failure.
    Terminal(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Repository(message) | Self::Keymap(message) | Self::Terminal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for RunError {}

/// Worktree root containing `path`, if any.
#[must_use]
pub fn find_repo_root(path: &Path) -> Option<PathBuf> {
    Repository::discover(path, CommandLog::default())
        .ok()
        .map(|repo| repo.root().to_owned())
}

/// Render the current state into plain text rows, without a terminal, and commit the frame's
/// hit regions as a presented frame would. Intended for tests and previews; styles are
/// discarded.
#[must_use]
pub fn render_text(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let buffer = render_buffer(app, width, height);
    (0..height)
        .map(|y| {
            buffer
                .row_symbols(y)
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// Render one frame headlessly into a cell buffer (symbols and styles), recording hit regions.
#[must_use]
pub fn render_buffer(app: &mut App, width: u16, height: u16) -> bmux_tui::buffer::Buffer {
    let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    {
        let mut cx = bmux_tui::paint::PaintCx::new(&mut frame);
        paint(app, &mut cx);
    }
    *app.hits.borrow_mut() = frame.hits().clone();
    drop(frame);
    buffer
}

/// Build application state without touching the terminal.
///
/// # Errors
/// * [`RunError::Repository`] when `path` is not inside a git worktree.
/// * [`RunError::Keymap`] when keybinding overrides or custom command keys are invalid.
pub fn build_app(options: RunOptions) -> Result<App, RunError> {
    let repo = Repository::discover(&options.path, CommandLog::default())
        .map_err(|error| RunError::Repository(error.to_string()))?;
    let loaded = options.config;
    let extra = loaded
        .ui
        .custom_commands
        .iter()
        .enumerate()
        .map(|(index, command)| bcode_keymap::KeymapAction {
            id: format!("bcode.git.custom.{index}"),
            scope: command.scope.clone(),
            description: command
                .description
                .clone()
                .unwrap_or_else(|| command.command.clone()),
            defaults: vec![command.key.clone()],
        })
        .collect();
    let keymap = Keymap::new(extra, loaded.keybindings.clone())
        .map_err(|error| RunError::Keymap(format!("invalid git keybindings: {error}")))?;
    let (theme, theme_error) = GitTheme::load(&loaded.theme, &loaded.config_dir, repo.root());
    let mut diagnostics = loaded.diagnostics.clone();
    diagnostics.extend(theme_error);
    let mut app = App::new(repo, loaded.ui, keymap, theme, options.host, diagnostics);
    app.env_message = options.commit_message;
    // Layouts are remembered for the directory the client was started in.
    if app.config.layout.remember {
        let launch = std::path::absolute(&options.path).unwrap_or_else(|_| options.path.clone());
        let store = options
            .state_dir
            .or_else(crate::panels::default_store_dir)
            .map(|dir| crate::panels::store_file(&dir, &launch));
        app.panels = crate::panels::Panels::new(&app.config.layout, &app.layout.windows, store);
        app.resume_session();
    }
    // Includes problems found while resolving the layout and commit settings.
    if let Some(first) = app.diagnostics.first().cloned() {
        app.toast(first, true);
    }
    Ok(app)
}

fn paint(app: &mut App, cx: &mut bmux_tui::paint::PaintCx<'_, '_>) {
    let area = cx.area();
    app.size.set((area.width, area.height));
    let frame = Frame::new(app);
    let root = tree(app, &frame);
    let layout = root.layout(
        Constraints::tight(Size::new(area.width, area.height)),
        &mut LayoutCx::default(),
    );
    root.paint(&layout, cx);
    // Pointer input reaches panel dividers through the areas this frame placed them in.
    let placed = bmux_tui::component::EventCx::new(&layout);
    *app.panels.areas.borrow_mut() = crate::panels::GROUPS
        .iter()
        .filter_map(|id| {
            placed
                .find_visible_rect(&bmux_tui::component::LayoutId::new(*id))
                .map(|area| (*id, area))
        })
        .collect();
    // Pointer input inside the popup's panel belongs to the popup.
    app.popup_area
        .set(placed.find_visible_rect(&bmux_tui::component::LayoutId::new("popup.surface")));
}

fn commit(app: &mut App, hits: &bmux_tui::hit::HitMap, _focus: &bmux_tui::focus::FocusTrap) {
    *app.hits.borrow_mut() = hits.clone();
}

type Presenter = TerminalPresenter<
    std::io::Stdout,
    fn(&mut App, &mut bmux_tui::paint::PaintCx<'_, '_>),
    fn(&mut App, &bmux_tui::hit::HitMap, &bmux_tui::focus::FocusTrap),
>;

/// Run the UI until the user quits.
///
/// # Errors
/// * [`RunError`] for repository, keybinding, terminal or runtime failures.
pub async fn run(options: RunOptions) -> Result<(), RunError> {
    let mut app = build_app(options)?;
    loop {
        let (returned, error) = Box::pin(session(app)).await?;
        app = returned;
        if let Some(error) = error {
            return Err(RunError::Terminal(error));
        }
        match app.take_external() {
            Some(external) if !app.quitting() => {
                run_external(&external.command, &external.cwd, external.pause);
                app.main_requested = None;
            }
            _ => break,
        }
    }
    for diagnostic in &app.diagnostics {
        if diagnostic.starts_with("input") {
            eprintln!("bcode git: {diagnostic}");
        }
    }
    Ok(())
}

/// One terminal session; returns when the user quits or an external program is requested.
async fn session(mut app: App) -> Result<(App, Option<String>), RunError> {
    let terminal_error = |error: std::io::Error| RunError::Terminal(error.to_string());
    let guard = CrosstermTerminalGuard::enter(std::io::stdout()).map_err(terminal_error)?;
    let size = terminal_size().map_err(terminal_error)?;
    let terminal = Terminal::new(std::io::stdout(), Rect::new(0, 0, size.width, size.height));
    let presenter: Presenter = TerminalPresenter::with_commit(terminal, paint, commit);
    let watcher = if app.config.refresh.watch {
        watch(app.repo.paths())
    } else {
        None
    };
    app.set_watching(watcher.is_some());
    let (runtime, handle) = Runtime::new(app, presenter, RuntimeConfig::default());

    let input_handle = handle.clone();
    let input = tokio::spawn(async move {
        let mut input = bmux_tui_runtime::ManagedTerminalInput::start(64);
        loop {
            match input.recv().await {
                Some(Ok(Some(event))) => {
                    if input_handle.send_terminal(event).await.is_err() {
                        break;
                    }
                }
                Some(Ok(None)) => {}
                Some(Err(error)) => {
                    let _ = input_handle
                        .send(Msg::InputFailed(format!("input: {error}")))
                        .await;
                    break;
                }
                None => break,
            }
        }
        input.shutdown().await;
    });
    let tick_handle = handle.clone();
    let tick = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(120)).await;
            if tick_handle.send(Msg::Tick).await.is_err() {
                break;
            }
        }
    });
    let forward = watcher.map(|(watcher, mut receiver)| {
        let watch_handle = handle.clone();
        let task = tokio::spawn(async move {
            while receiver.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                while receiver.try_recv().is_ok() {}
                if watch_handle.send(Msg::Watch).await.is_err() {
                    break;
                }
            }
        });
        (watcher, task)
    });
    let _ = handle.send(Msg::Watch).await;

    let result = runtime.run().await;
    input.abort();
    let _ = input.await;
    tick.abort();
    if let Some((watcher, task)) = forward {
        drop(watcher);
        task.abort();
    }
    let (app, error) = match result {
        Ok(output) => (output.program, None),
        Err(bmux_tui_runtime::RuntimeError::Program { error, output, .. }) => {
            (output.program, Some(error))
        }
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, output, .. }) => {
            (output.program, Some(error.to_string()))
        }
    };
    drop(guard);
    Ok((app, error))
}

/// Watch the worktree (including the git directory), signalling on relevant changes.
/// Watch the worktree and, for linked worktrees, the git directories that live outside it.
fn watch(
    paths: &bcode_git::models::RepoPaths,
) -> Option<(
    notify::RecommendedWatcher,
    tokio::sync::mpsc::UnboundedReceiver<()>,
)> {
    use notify::{RecursiveMode, Watcher};
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let relevant = event.paths.iter().any(|path| {
            let text = path.to_string_lossy();
            !(text.contains("/.git/objects/")
                || text.ends_with(".lock")
                || text.contains("/.git/logs/")
                || text.contains("/target/")
                || text.contains("/node_modules/"))
        });
        if relevant {
            let _ = sender.send(());
        }
    })
    .ok()?;
    watcher.watch(&paths.root, RecursiveMode::Recursive).ok()?;
    // A linked worktree's index and HEAD live in its own git directory, and branch refs in the
    // shared one; both sit outside the worktree. Watching them is best-effort.
    if !paths.git_dir.starts_with(&paths.root) {
        let _ = watcher.watch(&paths.git_dir, RecursiveMode::NonRecursive);
    }
    if !paths.common_dir.starts_with(&paths.root) {
        let _ = watcher.watch(&paths.common_dir, RecursiveMode::NonRecursive);
        let _ = watcher.watch(&paths.common_dir.join("refs"), RecursiveMode::Recursive);
    }
    Some((watcher, receiver))
}

fn run_external(command: &str, cwd: &Path, pause: bool) {
    let status = crate::ops::shell_command(command).current_dir(cwd).status();
    if let Err(error) = &status {
        eprintln!("bcode git: failed to run {command}: {error}");
    }
    if pause || status.as_ref().is_ok_and(|status| !status.success()) {
        use std::io::{BufRead as _, Write as _};
        print!("\nPress enter to return to bcode git… ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
}
