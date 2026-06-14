//! Local Ralph loop state management for the TUI.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const RALPH_STATE_SUBDIR: &str = "ralph";
const PROGRESS_DOC_FILE_NAME: &str = "progress.md";
const LOOP_METADATA_FILE_NAME: &str = "loop.json";

/// Created Ralph loop state paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRalphLoopState {
    /// Directory containing this Ralph loop's local state.
    pub state_dir: PathBuf,
    /// Canonical progress document path.
    pub progress_doc_path: PathBuf,
    /// Loop metadata path.
    pub metadata_path: PathBuf,
}

/// Create initial local state for a Ralph loop.
///
/// # Errors
///
/// Returns an error when the local state directory or files cannot be written,
/// or when loop metadata cannot be encoded.
pub fn create_initial_loop_state(
    loop_name: &str,
    repo_root: &Path,
    session_title: Option<&str>,
) -> Result<CreatedRalphLoopState, RalphStateError> {
    let paths = allocate_loop_paths(loop_name, repo_root)?;
    std::fs::create_dir_all(&paths.state_dir)?;
    let metadata = LoopMetadata::new(loop_name, repo_root, &paths);
    std::fs::write(
        &paths.metadata_path,
        serde_json::to_vec_pretty(&metadata).map_err(RalphStateError::Json)?,
    )?;
    std::fs::write(
        &paths.progress_doc_path,
        initial_progress_doc(loop_name, repo_root, session_title, &paths),
    )?;
    Ok(paths)
}

/// Return the default Ralph state root for a repository.
#[must_use]
pub fn repo_state_root(repo_root: &Path) -> PathBuf {
    bcode_config::default_state_dir()
        .join(RALPH_STATE_SUBDIR)
        .join(repo_state_id(repo_root))
}

fn allocate_loop_paths(
    loop_name: &str,
    repo_root: &Path,
) -> Result<CreatedRalphLoopState, RalphStateError> {
    let root = repo_state_root(repo_root);
    let loop_slug = slugify(loop_name);
    for suffix in 0..100_u8 {
        let candidate_slug = if suffix == 0 {
            loop_slug.clone()
        } else {
            format!("{loop_slug}-{suffix}")
        };
        let state_dir = root.join(candidate_slug);
        if !state_dir.exists() {
            return Ok(CreatedRalphLoopState {
                progress_doc_path: state_dir.join(PROGRESS_DOC_FILE_NAME),
                metadata_path: state_dir.join(LOOP_METADATA_FILE_NAME),
                state_dir,
            });
        }
    }
    Err(RalphStateError::LoopNameExhausted(loop_name.to_owned()))
}

#[derive(Debug, Serialize)]
struct LoopMetadata<'a> {
    loop_name: &'a str,
    loop_slug: String,
    repo_root: &'a Path,
    repo_id: String,
    progress_doc_path: &'a Path,
    status: &'static str,
    iteration_count: u64,
    created_at_ms: u128,
    updated_at_ms: u128,
}

impl<'a> LoopMetadata<'a> {
    fn new(loop_name: &'a str, repo_root: &'a Path, paths: &'a CreatedRalphLoopState) -> Self {
        let now_ms = now_ms();
        Self {
            loop_name,
            loop_slug: paths
                .state_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ralph-loop")
                .to_owned(),
            repo_root,
            repo_id: repo_state_id(repo_root),
            progress_doc_path: &paths.progress_doc_path,
            status: "created",
            iteration_count: 0,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }
}

fn initial_progress_doc(
    loop_name: &str,
    repo_root: &Path,
    session_title: Option<&str>,
    paths: &CreatedRalphLoopState,
) -> String {
    let session_title = session_title.unwrap_or("Untitled session");
    format!(
        "# Ralph Loop: {loop_name}\n\n\
         ## Purpose\n\n\
         Track Ralph loop progress captured from Bcode session `{session_title}`.\n\n\
         ## Current status\n\n\
         - **State:** Created\n\
         - **Repository:** `{repo_root}`\n\n\
         ## Definition of done\n\n\
         - [ ] Capture the intended goal, constraints, and non-goals from the current conversation.\n\
         - [ ] Confirm or create the isolated work area for this Ralph loop.\n\
         - [ ] Implement the planned changes in bounded iterations.\n\
         - [ ] Audit the repository state against this progress doc.\n\
         - [ ] Run relevant validation and record the results.\n\n\
         ## Practical checklist\n\n\
         - [ ] Replace this starter checklist with context-specific work items before running automated loop iterations.\n\
         - [ ] Keep completed work checked only after it is actually verified.\n\n\
         ## Decisions\n\n\
         - Ralph created this progress doc in Bcode state, outside the repository.\n\n\
         ## Blockers and questions\n\n\
         - [ ] Confirm the generated checklist reflects the goal before starting long-running work.\n\n\
         ## Session handoff notes\n\n\
         - Canonical progress doc path: `{progress_doc}`\n\
         - Ralph state directory: `{state_dir}`\n",
        repo_root = repo_root.display(),
        progress_doc = paths.progress_doc_path.display(),
        state_dir = paths.state_dir.display()
    )
}

fn repo_state_id(repo_root: &Path) -> String {
    slugify(&repo_root.to_string_lossy())
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "ralph-loop".to_owned()
    } else {
        slug
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

/// Ralph local state errors.
#[derive(Debug, thiserror::Error)]
pub enum RalphStateError {
    /// State I/O failed.
    #[error("Ralph state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// State metadata JSON encoding failed.
    #[error("Ralph state JSON failed: {0}")]
    Json(serde_json::Error),
    /// Could not allocate a unique loop state directory.
    #[error("could not allocate a unique Ralph loop state directory for {0}")]
    LoopNameExhausted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes_loop_names() {
        assert_eq!(slugify("Session Import Cleanup"), "session-import-cleanup");
        assert_eq!(slugify("  ...  "), "ralph-loop");
        assert_eq!(slugify("Ralph's Loop!"), "ralph-s-loop");
    }

    #[test]
    fn repo_state_root_uses_bcode_state_dir() {
        let root = repo_state_root(Path::new("/tmp/example repo"));
        assert!(root.ends_with(Path::new("ralph/tmp-example-repo")));
    }
}
