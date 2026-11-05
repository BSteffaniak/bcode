//! Conversion of a lazygit `config.yml` into equivalent bcode configuration.
//!
//! Only settings with a bcode counterpart are converted. Keybindings are emitted only where
//! they differ from bcode's defaults, so the result stays small and later default changes
//! still apply to everything the lazygit configuration did not customize.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::keymap::{Action, Keymap, Scope};

/// Result of a conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// TOML for a dedicated config file (or, nested, for `bcode.toml`), with notes as leading
    /// comments.
    pub toml: String,
    /// Settings or bindings that could not be converted faithfully.
    pub notes: Vec<String>,
}

/// Candidate lazygit configuration files, most specific first.
///
/// `LG_CONFIG_FILE` (comma separated) wins; otherwise lazygit's per-platform defaults.
#[must_use]
pub fn config_paths() -> Vec<PathBuf> {
    if let Ok(files) = std::env::var("LG_CONFIG_FILE") {
        return files
            .split(',')
            .filter(|file| !file.trim().is_empty())
            .map(|file| PathBuf::from(file.trim()))
            .collect();
    }
    let mut paths = Vec::new();
    if let Some(dir) = std::env::var_os("CONFIG_DIR") {
        paths.push(PathBuf::from(dir).join("config.yml"));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(dir).join("lazygit").join("config.yml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(
            home.join("Library")
                .join("Application Support")
                .join("lazygit")
                .join("config.yml"),
        );
        paths.push(home.join(".config").join("lazygit").join("config.yml"));
    }
    paths
}

/// Read and convert a lazygit configuration file: `path`, or the first of [`config_paths`]
/// that exists.
///
/// # Errors
/// * No configuration file was found, or it cannot be read or parsed.
pub fn import_file(path: Option<PathBuf>, nested: bool) -> Result<(PathBuf, Import), String> {
    let path = match path {
        Some(path) => path,
        None => config_paths()
            .into_iter()
            .find(|path| path.is_file())
            .ok_or("no lazygit config.yml found; pass its path explicitly")?,
    };
    let yaml =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let import = import(&yaml, nested).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok((path, import))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Lazygit {
    gui: Gui,
    git: Git,
    refresher: Refresher,
    keybinding: BTreeMap<String, BTreeMap<String, Keys>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Gui {
    side_panel_width: Option<f64>,
    expand_focused_side_panel: Option<bool>,
    expanded_side_panel_weight: Option<u16>,
    main_panel_split_mode: Option<String>,
    show_file_tree: Option<bool>,
    show_command_log: Option<bool>,
    command_log_size: Option<u16>,
    show_bottom_line: Option<bool>,
    screen_mode: Option<String>,
    portrait_mode: Option<String>,
    wrap_lines_in_diff_view: Option<bool>,
    use_hunk_mode_in_diff_view: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Git {
    diff_context_size: Option<u32>,
    skip_hook_prefix: Option<String>,
    commit: GitCommit,
    auto_fetch: Option<bool>,
    auto_refresh: Option<bool>,
    log: GitLog,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GitCommit {
    auto_wrap_commit_message: Option<bool>,
    auto_wrap_width: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GitLog {
    show_graph: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Refresher {
    refresh_interval: Option<u64>,
    fetch_interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Keys {
    One(String),
    Many(Vec<String>),
    // Unused or structured entries (for example `commitPrefixes`) are ignored.
    Other(serde::de::IgnoredAny),
}

/// Convert lazygit YAML into git UI settings: a dedicated `bcode-git` config file, or with
/// `nested` the same settings under their `bcode.toml` section.
///
/// # Errors
/// * The YAML cannot be parsed into lazygit's configuration shape.
pub fn import(yaml: &str, nested: bool) -> Result<Import, String> {
    let config: Lazygit = if yaml.trim().is_empty() {
        Lazygit::default()
    } else {
        yaml_serde::from_str(yaml).map_err(|error| format!("invalid lazygit config: {error}"))?
    };
    let mut notes = Vec::new();
    let mut ui = toml::Table::new();
    let mut layout = toml::Table::new();
    let mut files = toml::Table::new();
    let mut refresh = toml::Table::new();
    convert_settings(
        &config,
        &mut ui,
        &mut layout,
        &mut files,
        &mut refresh,
        &mut notes,
    );
    if !layout.is_empty() {
        ui.insert("layout".into(), layout.into());
    }
    if !files.is_empty() {
        ui.insert("files".into(), files.into());
    }
    if !refresh.is_empty() {
        ui.insert("refresh".into(), refresh.into());
    }
    prune_defaults(&mut ui, &[]);
    let scopes = convert_keys(&config.keybinding, &mut notes);
    if !scopes.is_empty() {
        ui.insert(
            "keybindings".into(),
            toml::Value::Table(
                scopes
                    .into_iter()
                    .map(|(scope, bindings)| {
                        (
                            scope,
                            toml::Value::Table(
                                bindings
                                    .into_iter()
                                    .map(|(key, action)| (key, toml::Value::String(action)))
                                    .collect(),
                            ),
                        )
                    })
                    .collect(),
            ),
        );
    }
    // The dedicated file's root is the git UI schema; `bcode.toml` nests the same table.
    let mut root = ui;
    if nested {
        for key in crate::config::BCODE_SECTION.iter().rev() {
            let mut parent = toml::Table::new();
            parent.insert((*key).to_owned(), toml::Value::Table(root));
            root = parent;
        }
    }
    let body = toml::to_string(&root).map_err(|error| error.to_string())?;
    let mut text = String::from("# Converted from lazygit configuration by bcode-git.\n");
    for note in &notes {
        text.push_str("# note: ");
        text.push_str(note);
        text.push('\n');
    }
    if !body.is_empty() {
        text.push('\n');
        text.push_str(&body);
    }
    Ok(Import { toml: text, notes })
}

fn convert_settings(
    config: &Lazygit,
    ui: &mut toml::Table,
    layout: &mut toml::Table,
    files: &mut toml::Table,
    refresh: &mut toml::Table,
    notes: &mut Vec<String>,
) {
    let gui = &config.gui;
    if let Some(width) = gui.side_panel_width {
        // Lazygit stores a fraction of the screen width.
        let percent = (width * 100.0).round().clamp(10.0, 90.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        layout.insert("side_panel_width".into(), (percent as i64).into());
    }
    if let Some(mode) = &gui.screen_mode {
        match mode.as_str() {
            "normal" | "half" | "full" => {
                layout.insert("screen_mode".into(), mode.clone().into());
            }
            other => notes.push(format!("gui.screenMode \"{other}\" is not supported")),
        }
    }
    if let Some(expand) = gui.expand_focused_side_panel {
        layout.insert("expand_focused".into(), expand.into());
    }
    if let Some(weight) = gui.expanded_side_panel_weight {
        layout.insert("expanded_weight".into(), i64::from(weight).into());
    }
    if let Some(mode) = &gui.main_panel_split_mode {
        // Lazygit's "horizontal" splits the window with a vertical divider: panes side by side.
        let split = match mode.as_str() {
            "horizontal" => Some("side-by-side"),
            "vertical" => Some("stacked"),
            "flexible" => Some("flexible"),
            _ => None,
        };
        match split {
            Some(split) => {
                layout.insert("main_split".into(), split.into());
            }
            None => notes.push(format!(
                "gui.mainPanelSplitMode \"{mode}\" is not supported"
            )),
        }
    }
    if let Some(mode) = &gui.portrait_mode {
        match mode.as_str() {
            "auto" | "always" | "never" => {
                layout.insert("portrait".into(), mode.clone().into());
            }
            other => notes.push(format!("gui.portraitMode \"{other}\" is not supported")),
        }
    }
    if gui.show_command_log.is_some() || gui.command_log_size.is_some() {
        let mut log = toml::Table::new();
        if let Some(show) = gui.show_command_log {
            log.insert("show".into(), show.into());
        }
        if let Some(size) = gui.command_log_size {
            log.insert("height".into(), i64::from(size).into());
        }
        layout.insert("command_log".into(), log.into());
    }
    if let Some(show) = gui.show_bottom_line {
        layout.insert("status_bar".into(), show.into());
    }
    if let Some(tree) = gui.show_file_tree {
        files.insert("tree".into(), tree.into());
    }
    convert_editing(config, ui);
    let git = &config.git;
    if let Some(context) = git.diff_context_size {
        ui.insert("diff_context".into(), i64::from(context).into());
    }
    if let Some(graph) = &git.log.show_graph {
        ui.insert("show_graph".into(), (graph != "never").into());
    }
    match (git.auto_fetch, config.refresher.fetch_interval) {
        (Some(false), _) => {
            refresh.insert("auto_fetch_seconds".into(), 0.into());
        }
        (_, Some(seconds)) if git.auto_fetch == Some(true) => {
            refresh.insert(
                "auto_fetch_seconds".into(),
                i64::try_from(seconds).unwrap_or(i64::MAX).into(),
            );
        }
        _ => {}
    }
    match (git.auto_refresh, config.refresher.refresh_interval) {
        (Some(false), _) => {
            refresh.insert("poll_seconds".into(), 0.into());
        }
        (_, Some(seconds)) if git.auto_refresh == Some(true) => {
            refresh.insert(
                "poll_seconds".into(),
                i64::try_from(seconds).unwrap_or(i64::MAX).into(),
            );
        }
        _ => {}
    }
}

/// Diff wrapping, staging hunk mode, and commit message wrapping and hook skipping.
fn convert_editing(config: &Lazygit, ui: &mut toml::Table) {
    let gui = &config.gui;
    let mut section = |name: &str, key: &str, value: toml::Value| {
        if let toml::Value::Table(table) = ui
            .entry(name.to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        {
            table.insert(key.to_owned(), value);
        }
    };
    if let Some(wrap) = gui.wrap_lines_in_diff_view {
        section("diff", "wrap_lines", wrap.into());
    }
    if let Some(hunk) = gui.use_hunk_mode_in_diff_view {
        section("staging", "hunk_mode", hunk.into());
    }
    let git = &config.git;
    match (
        git.commit.auto_wrap_commit_message,
        git.commit.auto_wrap_width,
    ) {
        (Some(false), _) => section("commit", "wrap_width", 0.into()),
        (_, Some(width)) => section("commit", "wrap_width", i64::from(width).into()),
        _ => {}
    }
    if let Some(prefix) = git
        .skip_hook_prefix
        .as_ref()
        .filter(|prefix| !prefix.is_empty())
    {
        section("commit", "skip_hook_prefix", prefix.clone().into());
    }
}

/// Remove settings equal to bcode's defaults, judged by decoding each one on its own.
fn prune_defaults(table: &mut toml::Table, path: &[String]) {
    let keys: Vec<String> = table.keys().cloned().collect();
    for key in keys {
        let mut leaf_path = path.to_vec();
        leaf_path.push(key.clone());
        if let Some(toml::Value::Table(child)) = table.get_mut(&key) {
            prune_defaults(child, &leaf_path);
            if child.is_empty() {
                table.remove(&key);
            }
            continue;
        }
        let mut single = toml::Table::new();
        single.insert(key.clone(), table[&key].clone());
        for segment in path.iter().rev() {
            let mut parent = toml::Table::new();
            parent.insert(segment.clone(), single.into());
            single = parent;
        }
        let is_default = single
            .try_into::<crate::config::GitUiConfig>()
            .is_ok_and(|decoded| decoded == crate::config::GitUiConfig::default());
        if is_default {
            table.remove(&key);
        }
    }
}

/// Lazygit bindings by `section.name` (alternates such as `name-alt1` share the base name).
// A flat lookup table: one arm per lazygit binding reads better than any split.
#[allow(clippy::too_many_lines)]
fn lazygit_actions(section: &str, name: &str) -> &'static [Action] {
    use Action as A;
    match (section, name) {
        ("universal", "quit") => &[A::Quit],
        ("universal", "return") => &[A::Back],
        ("universal", "togglePanel" | "nextBlock") => &[A::NextPanel],
        ("universal", "prevBlock") => &[A::PreviousPanel],
        ("universal", "prevItem") => &[A::Up, A::MenuUp, A::MainLineUp, A::StageLineUp],
        ("universal", "nextItem") => &[A::Down, A::MenuDown, A::MainLineDown, A::StageLineDown],
        ("universal", "prevPage") => &[A::PageUp],
        ("universal", "nextPage") => &[A::PageDown],
        ("universal", "gotoTop") => &[A::Top],
        ("universal", "gotoBottom") => &[A::Bottom],
        ("universal", "focusMainView") => &[A::FocusMain],
        ("universal", "startSearch") => &[A::Filter],
        ("universal", "optionMenu") => &[A::Help],
        ("universal", "select") => &[
            A::FileToggleStaged,
            A::WorktreeSwitch,
            A::BranchCheckout,
            A::RemoteBranchCheckout,
            A::TagCheckout,
            A::ReflogCheckout,
            A::StashApply,
            A::CommitFileTogglePatch,
            A::StageToggle,
        ],
        ("universal", "goInto") => &[
            A::FileEnter,
            A::SubmoduleEnter,
            A::BranchViewCommits,
            A::RemoteView,
            A::RemoteBranchViewCommits,
            A::TagViewCommits,
            A::CommitViewFiles,
            A::ReflogViewFiles,
            A::StashViewFiles,
            A::CommitFileView,
        ],
        ("universal", "confirmMenu") => &[A::MenuConfirm],
        ("universal", "confirm") => &[A::ConfirmYes],
        ("universal", "submitEditorText") => &[A::InputSubmit],
        ("universal", "remove") => &[
            A::FileDiscard,
            A::WorktreeRemove,
            A::BranchDelete,
            A::RemoteDelete,
            A::RemoteBranchDelete,
            A::TagDelete,
            A::CommitDrop,
            A::StashDrop,
            A::StageDiscard,
        ],
        ("universal", "new") => &[
            A::WorktreeNew,
            A::BranchNew,
            A::RemoteNew,
            A::TagNew,
            A::CommitNewBranch,
            A::StashBranch,
        ],
        ("universal", "edit") => &[
            A::StatusOpenConfig,
            A::FileEdit,
            A::RemoteEdit,
            A::CommitEdit,
            A::StageEdit,
        ],
        ("universal", "openFile") => &[A::FileOpen],
        ("universal", "scrollUpMain") => &[A::ScrollMainUp],
        ("universal", "scrollDownMain") => &[A::ScrollMainDown],
        ("universal", "executeShellCommand") => &[A::ShellCommand],
        ("universal", "createRebaseOptionsMenu") => &[A::OperationMenu],
        ("universal", "pushFiles") => &[A::Push],
        ("universal", "pullFiles") => &[A::Pull],
        ("universal", "refresh") => &[A::Refresh],
        ("universal", "nextTab") => &[A::NextTab],
        ("universal", "prevTab") => &[A::PreviousTab],
        ("universal", "nextScreenMode") => &[A::ScreenModeNext],
        ("universal", "prevScreenMode") => &[A::ScreenModePrevious],
        ("universal", "undo") => &[A::Undo],
        ("universal", "redo") => &[A::Redo],
        ("universal", "diffingMenu") => &[A::DiffingMenu],
        ("universal", "copyToClipboard") => &[A::CopyToClipboard],
        ("universal", "extrasMenu") => &[A::CommandLog],
        ("universal", "increaseContextInDiffView") => &[A::DiffContextMore],
        ("universal", "decreaseContextInDiffView") => &[A::DiffContextLess],
        ("universal", "toggleRangeSelect") => &[A::StageRange],
        ("files", "commitChanges") => &[A::FileCommit, A::StageCommit],
        ("files", "commitChangesWithoutHook") => &[A::FileCommitNoVerify],
        ("files", "amendLastCommit") => &[A::FileAmend],
        ("files", "commitChangesWithEditor") => &[A::FileCommitEditor],
        ("files", "ignoreFile") => &[A::FileIgnore],
        ("files", "stashAllChanges") => &[A::FileStash],
        ("files", "viewStashOptions") => &[A::FileStashMenu],
        ("files", "toggleStagedAll") => &[A::FileStageAll],
        ("files", "viewResetOptions") => &[A::FileResetMenu],
        ("files", "fetch") => &[A::Fetch],
        ("files", "toggleTreeView") => &[A::FileToggleTree],
        ("files", "collapseAll") => &[A::FileCollapseAll],
        ("files", "expandAll") => &[A::FileExpandAll],
        ("branches", "checkoutBranchByName") => &[A::BranchCheckoutByName],
        ("branches", "rebaseBranch") => &[A::BranchRebase, A::RemoteBranchRebase],
        ("branches", "renameBranch") => &[A::BranchRename],
        ("branches", "mergeIntoCurrentBranch") => &[A::BranchMerge, A::RemoteBranchMerge],
        ("branches", "fastForward") => &[A::BranchFastForward],
        ("branches", "createTag") => &[A::BranchTag],
        ("branches", "pushTag") => &[A::TagPush],
        ("branches", "setUpstream") => &[A::BranchUpstream, A::RemoteBranchUpstream],
        ("branches", "fetchRemote") => &[A::RemoteFetch],
        ("commits", "squashDown") => &[A::CommitSquash],
        ("commits", "renameCommit") => &[A::CommitReword],
        ("commits", "viewResetOptions") => &[A::CommitResetMenu, A::ReflogResetMenu],
        ("commits", "markCommitAsFixup") => &[A::CommitFixup],
        ("commits", "createFixupCommit") => &[A::CommitCreateFixup],
        ("commits", "squashAboveCommits") => &[A::CommitAutosquash],
        ("commits", "moveDownCommit") => &[A::CommitMoveDown],
        ("commits", "moveUpCommit") => &[A::CommitMoveUp],
        ("commits", "amendToCommit") => &[A::CommitAmendInto],
        ("commits", "revertCommit") => &[A::CommitRevert],
        ("commits", "cherryPickCopy") => &[A::CommitCherryPickCopy],
        ("commits", "pasteCommits") => &[A::CommitCherryPickPaste],
        ("commits", "tagCommit") => &[A::CommitTag],
        ("commits", "checkoutCommit") => &[A::CommitCheckout],
        ("commits", "copyCommitAttributeToClipboard") => &[A::CommitCopySha],
        ("commits", "viewBisectOptions") => &[A::CommitBisect],
        ("stash", "popStash") => &[A::StashPop],
        ("stash", "renameStash") => &[A::StashRename],
        ("commitFiles", "checkoutCommitFile") => &[A::CommitFileCheckout],
        ("main", "toggleSelectHunk") => &[A::StageHunkMode],
        ("submodules", "update") => &[A::SubmoduleUpdate],
        _ => &[],
    }
}

const WINDOW_ACTIONS: [Action; 9] = [
    Action::FocusWindow1,
    Action::FocusWindow2,
    Action::FocusWindow3,
    Action::FocusWindow4,
    Action::FocusWindow5,
    Action::FocusWindow6,
    Action::FocusWindow7,
    Action::FocusWindow8,
    Action::FocusWindow9,
];

/// Strip lazygit's alternate suffixes: `prevItem-alt`, `quit-alt1`, `nextBlock-alt2`.
fn base_name(name: &str) -> &str {
    name.find("-alt").map_or(name, |index| &name[..index])
}

/// Translate a lazygit key spelling into bcode's. `None` for `<disabled>` or unknown names.
fn convert_key(key: &str) -> Option<String> {
    let Some(inner) = key
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        let mut chars = key.chars();
        return match (chars.next(), chars.next()) {
            (Some(' '), None) => Some("space".into()),
            (Some(ch), None) if ch.is_ascii_uppercase() => {
                Some(format!("shift+{}", ch.to_ascii_lowercase()))
            }
            (Some(_), None) => Some(key.to_owned()),
            _ => None,
        };
    };
    if inner == "disabled" {
        return None;
    }
    let mut modifiers = Vec::new();
    let mut rest = inner;
    loop {
        if let Some(stripped) = rest.strip_prefix("c-") {
            modifiers.push("ctrl");
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("a-") {
            modifiers.push("alt");
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("s-") {
            modifiers.push("shift");
            rest = stripped;
        } else {
            break;
        }
    }
    let name = match rest {
        "esc" => "escape",
        "backtab" => {
            modifiers.push("shift");
            "tab"
        }
        "pgup" => "pageup",
        "pgdown" => "pagedown",
        "space" | "enter" | "tab" | "up" | "down" | "left" | "right" | "home" | "end"
        | "backspace" | "delete" | "insert" => rest,
        other if other.chars().count() == 1 => other,
        other if other.starts_with('f') && other[1..].parse::<u8>().is_ok() => other,
        _ => return None,
    };
    let mut spelled: Vec<&str> = modifiers;
    spelled.push(name);
    let spelled = spelled.join("+");
    bmux_keyboard::parse_key_stroke(&spelled)
        .is_ok()
        .then_some(spelled)
}

fn key_list(keys: &Keys) -> Vec<&str> {
    match keys {
        Keys::One(key) => vec![key.as_str()],
        Keys::Many(keys) => keys.iter().map(String::as_str).collect(),
        Keys::Other(_) => Vec::new(),
    }
}

fn convert_keys(
    sections: &BTreeMap<String, BTreeMap<String, Keys>>,
    notes: &mut Vec<String>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    // Every key lazygit binds to each bcode action.
    // Every key lazygit binds to each bcode action, with whether it came from a panel section
    // (which takes priority over `universal`, as in lazygit).
    let mut wanted: BTreeMap<Action, BTreeMap<String, bool>> = BTreeMap::new();
    let mut unknown = BTreeSet::new();
    for (section, bindings) in sections {
        for (name, keys) in bindings {
            let base = base_name(name);
            if section == "universal" && base == "jumpToBlock" {
                for (action, key) in WINDOW_ACTIONS.iter().zip(key_list(keys)) {
                    if let Some(key) = convert_key(key) {
                        wanted.entry(*action).or_default().insert(key, false);
                    }
                }
                continue;
            }
            let actions = lazygit_actions(section, base);
            for key in key_list(keys) {
                if key == "<disabled>" {
                    // Disabling one alternate leaves the others; an action only loses all
                    // keys if every spelling is disabled, handled by the empty set below.
                    for action in actions {
                        wanted.entry(*action).or_default();
                    }
                    continue;
                }
                match convert_key(key) {
                    Some(key) => {
                        for action in actions {
                            wanted
                                .entry(*action)
                                .or_default()
                                .insert(key.clone(), section != "universal");
                        }
                    }
                    None => notes.push(format!(
                        "{section}.{name}: key \"{key}\" has no bcode spelling"
                    )),
                }
            }
            if actions.is_empty() && !matches!(keys, Keys::Other(_)) {
                unknown.insert(format!("{section}.{name}"));
            }
        }
    }
    if !unknown.is_empty() {
        notes.push(format!(
            "no bcode equivalent for: {}",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    // Emit only actions whose lazygit keys are not already among bcode's defaults; an
    // override replaces every default key of the action in its scope.
    let mut scopes: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut priority: BTreeMap<(&str, String), bool> = BTreeMap::new();
    for (action, keys) in &wanted {
        let defaults: BTreeSet<&str> = action.defaults().iter().copied().collect();
        let empty_and_unbound = keys.is_empty() && defaults.is_empty();
        if empty_and_unbound
            || (!keys.is_empty() && keys.keys().all(|key| defaults.contains(key.as_str())))
        {
            continue;
        }
        let scope = scopes.entry(action.scope().name().to_owned()).or_default();
        if keys.is_empty() {
            // Every lazygit spelling is disabled: unbind bcode's defaults.
            for key in &defaults {
                scope.entry((*key).to_owned()).or_default();
            }
            continue;
        }
        for (key, specific) in keys {
            let existing = scope
                .get(key)
                .filter(|existing| !existing.is_empty() && **existing != action.id())
                .cloned();
            let Some(existing) = existing else {
                scope.insert(key.clone(), action.id());
                priority.insert((action.scope().name(), key.clone()), *specific);
                continue;
            };
            let existing_specific = priority
                .get(&(action.scope().name(), key.clone()))
                .copied()
                .unwrap_or(false);
            let (kept, dropped) = if *specific && !existing_specific {
                scope.insert(key.clone(), action.id());
                priority.insert((action.scope().name(), key.clone()), true);
                (action.id(), existing)
            } else {
                (existing, action.id())
            };
            notes.push(format!(
                "\"{key}\" is bound to both {kept} and {dropped} in {}; kept {kept}",
                action.scope().name()
            ));
        }
    }
    report_shadowing(&scopes, notes);
    scopes
}

/// Universal lazygit bindings live in bcode's shared `git.list` / `git.global` scopes, which a
/// panel's own bindings take priority over. Report keys a panel default still claims.
fn report_shadowing(scopes: &BTreeMap<String, BTreeMap<String, String>>, notes: &mut Vec<String>) {
    let Ok(keymap) = Keymap::new(Vec::new(), scopes.clone()) else {
        notes.push("the converted keybindings conflict; review them before use".into());
        return;
    };
    let panels = [
        Scope::Status,
        Scope::Files,
        Scope::Worktrees,
        Scope::Submodules,
        Scope::Branches,
        Scope::Remotes,
        Scope::RemoteBranches,
        Scope::Tags,
        Scope::Commits,
        Scope::CommitFiles,
        Scope::Reflog,
        Scope::Stash,
    ];
    for shared in [Scope::List, Scope::Global] {
        let Some(bindings) = scopes.get(shared.name()) else {
            continue;
        };
        for (key, action) in bindings {
            if action.is_empty() {
                continue;
            }
            let Ok(stroke) = bmux_keyboard::parse_key_stroke(key) else {
                continue;
            };
            let shadowed: Vec<&str> = panels
                .iter()
                .filter(|panel| {
                    matches!(
                        keymap.resolve(&[**panel], &[stroke]),
                        crate::keymap::Resolution::Action(_)
                    )
                })
                .map(|panel| panel.name())
                .collect();
            if !shadowed.is_empty() {
                notes.push(format!(
                    "\"{key}\" ({action}) is used by a panel action in {}; rebind those to use it there",
                    shadowed.join(", ")
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read converted output the way a dedicated config file is read.
    fn loaded(import: &Import) -> crate::config::LoadedConfig {
        crate::config::from_tables(&toml::Table::new(), Some(&import.toml.parse().unwrap()))
    }

    fn bindings(import: &Import) -> BTreeMap<String, BTreeMap<String, String>> {
        loaded(import).keybindings
    }

    #[test]
    fn converts_layout_and_file_settings() {
        let import = import(
            "gui:\n  sidePanelWidth: 0.3333\n  screenMode: half\n  showFileTree: false\n  \
             mainPanelSplitMode: horizontal\n  showBottomLine: true\n  \
             wrapLinesInDiffView: false\n  useHunkModeInDiffView: true\n\
             git:\n  diffContextSize: 5\n  skipHookPrefix: WIP\n  \
             commit:\n    autoWrapCommitMessage: true\n    autoWrapWidth: 80\n",
            false,
        )
        .unwrap();
        let loaded = loaded(&import);
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(loaded.ui.layout.side_panel_width, 33);
        assert_eq!(
            loaded.ui.layout.screen_mode,
            crate::config::ScreenMode::Half
        );
        assert_eq!(
            loaded.ui.layout.main_split,
            crate::config::MainSplit::SideBySide
        );
        assert!(!loaded.ui.files.tree);
        assert_eq!(loaded.ui.diff_context, 5);
        assert!(!loaded.ui.diff.wrap_lines);
        assert!(loaded.ui.staging.hunk_mode);
        assert_eq!(loaded.ui.commit.wrap_width, 80);
        assert_eq!(loaded.ui.commit.skip_hook_prefix, "WIP");
    }

    #[test]
    fn remapped_keys_replace_defaults_and_defaults_are_omitted() {
        let import = import(
            "keybinding:\n  universal:\n    prevItem: <up>\n    nextItem: <down>\n    \
             prevItem-alt: n\n    nextItem-alt: t\n    quit: q\n    quit-alt1: <c-c>\n    \
             new: N\n    optionMenu: <disabled>\n    optionMenu-alt1: '?'\n  \
             commits:\n    moveDownCommit: <c-j>\n    pickCommit: p\n",
            false,
        )
        .unwrap();
        let scopes = bindings(&import);
        assert_eq!(scopes["git.list"]["n"], "bcode.git.list.up");
        assert_eq!(scopes["git.list"]["up"], "bcode.git.list.up");
        assert_eq!(scopes["git.branches"]["shift+n"], "bcode.git.branches.new");
        // Unchanged bindings (quit, help, move-down) are left to bcode's defaults.
        assert!(!scopes.contains_key("git.global"), "{scopes:?}");
        assert!(
            !scopes["git.commits"]
                .values()
                .any(|action| action.ends_with("move-down")),
            "{scopes:?}"
        );
        // The keymap accepts the result, and the remap takes effect.
        let keymap = Keymap::new(Vec::new(), scopes).unwrap();
        assert_eq!(keymap.key(Action::BranchNew), Some("shift+n"));
        // "t" is the revert key in the commits panel, so the universal remap is reported.
        assert!(
            import
                .notes
                .iter()
                .any(|note| note.contains("\"t\"") && note.contains("git.commits")),
            "{:?}",
            import.notes
        );
        assert!(
            import
                .notes
                .iter()
                .any(|note| note.contains("commits.pickCommit"))
        );
    }

    #[test]
    fn key_spellings_translate() {
        assert_eq!(convert_key("<c-c>").as_deref(), Some("ctrl+c"));
        assert_eq!(convert_key("<backtab>").as_deref(), Some("shift+tab"));
        assert_eq!(convert_key("<s-down>").as_deref(), Some("shift+down"));
        assert_eq!(convert_key("<a-enter>").as_deref(), Some("alt+enter"));
        assert_eq!(convert_key("<pgup>").as_deref(), Some("pageup"));
        assert_eq!(convert_key("A").as_deref(), Some("shift+a"));
        assert_eq!(convert_key(">").as_deref(), Some(">"));
        assert_eq!(convert_key("<disabled>"), None);
    }

    #[test]
    fn empty_input_yields_only_a_header() {
        let import = import("", false).unwrap();
        assert_eq!(import.notes, Vec::<String>::new());
        assert!(loaded(&import).keybindings.is_empty());
    }

    #[test]
    fn nested_output_is_the_same_settings_under_the_bcode_section() {
        let yaml = "gui:\n  screenMode: half\n  showFileTree: false\n\
                    keybinding:\n  universal:\n    prevItem-alt: n\n";
        let flat = loaded(&import(yaml, false).unwrap());
        let nested = crate::config::from_table(&import(yaml, true).unwrap().toml.parse().unwrap());
        assert_eq!(flat.ui, nested.ui);
        assert_eq!(flat.keybindings, nested.keybindings);
        assert_eq!(
            nested.ui.layout.screen_mode,
            crate::config::ScreenMode::Half
        );
        assert_eq!(nested.keybindings["git.list"]["n"], "bcode.git.list.up");
    }
}
