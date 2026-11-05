//! Commit editor prefill: finding the message, and watching its sources while the editor is
//! open so text written meanwhile still arrives (only into an empty editor).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_git::Repository;
use bcode_git::models::CommitMessageSource;
use bmux_tui_runtime::{Subscription, SubscriptionKey};

use crate::app::Msg;

/// Subscription identity of the source watcher.
pub const WATCH_KEY: &str = "commit-message-sources";

/// Writers often save in several steps (truncate, write, rename); wait for them to settle.
const SETTLE: Duration = Duration::from_millis(120);

/// Outcome of looking for a prefill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefill {
    /// The first source with text, and that text.
    pub chosen: Option<(CommitMessageSource, String)>,
    /// A git-prepared message (merge or squash) that a draft takes the place of.
    pub replaced: Option<CommitMessageSource>,
    /// Sources that could not be read.
    pub errors: Vec<String>,
}

/// Find the prefill among `sources` in order. `env` is the environment message, if it is still
/// available to this editor.
#[must_use]
pub fn find(repo: &Repository, sources: &[CommitMessageSource], env: Option<String>) -> Prefill {
    let mut prefill = Prefill::default();
    let mut env = env;
    for source in sources {
        let text = if *source == CommitMessageSource::Env {
            env.take()
        } else {
            match repo.commit_message_from(*source) {
                Ok(text) => text,
                Err(error) => {
                    prefill
                        .errors
                        .push(format!("{}: {}", source.label(), error.message()));
                    None
                }
            }
        };
        if let Some(text) = text {
            prefill.chosen = Some((*source, text));
            break;
        }
    }
    if prefill
        .chosen
        .as_ref()
        .is_some_and(|(source, _)| source.is_draft())
    {
        prefill.replaced = [CommitMessageSource::Merge, CommitMessageSource::Squash]
            .into_iter()
            .filter(|source| sources.contains(source))
            .find(|source| {
                repo.commit_message_from(*source)
                    .is_ok_and(|text| text.is_some())
            });
    }
    prefill
}

/// Wrap a commit message's description to `width` display columns at word boundaries.
///
/// The summary (first line) is kept whole, and lines that already fit, blank lines, and
/// indented lines (code, quoted output) are kept as written. `0` disables wrapping.
#[must_use]
pub fn wrap_description(message: &str, width: u16) -> String {
    use bmux_tui::text::{TextWrap, TextWrapGeometry, wrap_text};
    use bmux_tui::text_width::display_width;

    let width = usize::from(width);
    if width == 0 {
        return message.to_owned();
    }
    let mut lines = message.split('\n');
    let mut out: Vec<String> = lines.next().map(str::to_owned).into_iter().collect();
    for line in lines {
        if display_width(line) <= width || line.starts_with([' ', '\t']) {
            out.push(line.to_owned());
        } else {
            out.extend(wrap_text(
                line,
                TextWrapGeometry::uniform(width),
                TextWrap::Word,
            ));
        }
    }
    out.join("\n")
}

/// Files holding the file-backed `sources`, existing or not.
#[must_use]
pub fn watched_files(repo: &Repository, sources: &[CommitMessageSource]) -> Vec<PathBuf> {
    sources
        .iter()
        .filter_map(|source| repo.commit_message_path(*source).ok().flatten())
        .collect()
}

/// Watch `files` and report [`Msg::CommitSourcesChanged`] when any of them is written,
/// created or removed. The watcher lives until the subscription is cancelled.
#[must_use]
pub fn subscription(files: Vec<PathBuf>) -> Subscription<Msg> {
    Subscription::new(SubscriptionKey::new(WATCH_KEY), move |sender| async move {
        let Some((_watcher, mut changes)) = watch(&files) else {
            return;
        };
        while changes.recv().await.is_some() {
            tokio::time::sleep(SETTLE).await;
            while changes.try_recv().is_ok() {}
            if sender.send(Msg::CommitSourcesChanged).await.is_err() {
                return;
            }
        }
    })
}

/// A filesystem watcher on the folders holding `files` (which may not exist yet), sending one
/// unit per relevant event. `None` when nothing could be watched.
#[must_use]
pub fn watch(
    files: &[PathBuf],
) -> Option<(
    notify::RecommendedWatcher,
    tokio::sync::mpsc::UnboundedReceiver<()>,
)> {
    use notify::{RecursiveMode, Watcher};

    // Events may report resolved paths (for example `/private/var` for `/var`), so compare
    // canonical folders plus file names.
    let targets: BTreeSet<(PathBuf, std::ffi::OsString)> = files
        .iter()
        .filter_map(|file| Some((canonical_parent(file)?, file.file_name()?.to_owned())))
        .collect();
    let folders: BTreeSet<PathBuf> = targets.iter().map(|(folder, _)| folder.clone()).collect();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let relevant = event.paths.iter().any(|path| {
            let (Some(folder), Some(name)) = (canonical_parent(path), path.file_name()) else {
                return false;
            };
            targets.contains(&(folder, name.to_owned()))
        });
        if relevant {
            let _ = sender.send(());
        }
    })
    .ok()?;
    let mut watching = false;
    for folder in &folders {
        watching |= watcher.watch(folder, RecursiveMode::NonRecursive).is_ok();
    }
    watching.then_some((watcher, receiver))
}

fn canonical_parent(path: &Path) -> Option<PathBuf> {
    path.parent()?.canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_writes_to_watched_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let draft = dir.path().join("BCODE_GIT_COMMIT_MSG");
        let (_watcher, mut changes) = watch(std::slice::from_ref(&draft)).expect("watching");
        std::fs::write(dir.path().join("unrelated"), "x").unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(500), changes.recv()).await;
        assert!(quiet.is_err(), "an unrelated file must not count");
        std::fs::write(&draft, "message").unwrap();
        let change = tokio::time::timeout(Duration::from_secs(10), changes.recv()).await;
        assert_eq!(change.ok().flatten(), Some(()));
    }
}
