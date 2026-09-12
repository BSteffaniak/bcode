#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bounded, deterministic workflow package and standalone-source discovery.
//!
//! This package owns filesystem source discovery and confinement. It does not validate against the
//! live plugin catalog, persist authored state, publish revisions, or start runs.
//! Member suppression uses a private request-local temporary `SQLite` index with a bounded page
//! cache. It is never reopened or treated as canonical; normal drop removes its directory.
//! Abrupt process death may leave temporary files for operating-system cleanup.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum package or standalone candidates inspected by one discovery request.
///
/// Application catalog projection may request one additional candidate to determine whether a
/// public page capped at [`bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE`] has a
/// continuation.
pub const MAX_DISCOVERY_RESULTS: usize = bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE + 1;

/// Errors returned before a bounded discovery result can be produced.
#[derive(Debug, Error)]
pub enum WorkflowDiscoveryError {
    #[error("workflow discovery I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("workflow discovery configuration error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("workflow discovery source error: {0}")]
    Workflow(#[from] bcode_workflow::WorkflowError),
    #[error("workflow discovery JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workflow discovery TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("workflow discovery temporary index failure")]
    Index(#[from] rusqlite::Error),
    #[error("workflow discovery failed: {0}")]
    Invalid(String),
}

/// One secret-safe diagnostic for a source that could not become a launch candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiscoveryDiagnostic {
    pub source_label: String,
    pub path: PathBuf,
    pub code: String,
    pub message: String,
}

/// One confined discovered source before live-catalog validation and publication lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredWorkflowSource {
    Package {
        package_id: String,
        source_label: String,
        precedence: u32,
        manifest_path: PathBuf,
        closure: bcode_workflow::WorkflowPackageClosure,
    },
    Standalone {
        source_label: String,
        precedence: u32,
        source_path: PathBuf,
        source_format: bcode_workflow::WorkflowSourceFormat,
        source: String,
    },
}

impl DiscoveredWorkflowSource {
    /// Return the deterministic filesystem identity used to page discovery results.
    #[must_use]
    pub fn source_key(&self) -> String {
        match self {
            Self::Package {
                package_id,
                manifest_path,
                ..
            } => format!("package:{package_id}:{}", manifest_path.display()),
            Self::Standalone { source_path, .. } => {
                format!("source:{}", source_path.display())
            }
        }
    }

    /// Return configured source precedence; lower values win.
    #[must_use]
    pub const fn precedence(&self) -> u32 {
        match self {
            Self::Package { precedence, .. } | Self::Standalone { precedence, .. } => *precedence,
        }
    }
}

/// Sources produced by one bounded discovery advance.
///
/// Sources are reconciled but not globally sorted. Consumers must discard accumulated output
/// if a later advance fails; completion is authoritative only when `complete` is true.
#[derive(Debug)]
pub struct WorkflowDiscoveryBatch {
    /// Reconciled sources produced by this advance, at most the requested budget.
    pub sources: Vec<DiscoveredWorkflowSource>,
    /// Whether enumeration and reconciliation have finished.
    pub complete: bool,
    /// Bounded diagnostics, delivered only at completion.
    pub diagnostics: Vec<WorkflowDiscoveryDiagnostic>,
}

/// Complete bounded, non-mutating discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowDiscoveryResult {
    pub sources: Vec<DiscoveredWorkflowSource>,
    pub diagnostics: Vec<WorkflowDiscoveryDiagnostic>,
}

#[derive(Debug, Clone)]
struct DiscoveryRoot {
    path: PathBuf,
    label: String,
    precedence: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageImport {
    import_id: String,
    package_id: String,
    export: String,
    #[serde(default)]
    manifest: Option<String>,
    #[serde(default)]
    target: Option<bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    package_lock_digest_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageManifest {
    version: u32,
    package_id: String,
    exports: BTreeMap<String, String>,
    #[serde(default)]
    external_dependencies: BTreeMap<String, bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    imports: Vec<SourcePackageImport>,
    members: Vec<SourcePackageMember>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageMember {
    member_id: String,
    source_name: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    external_dependencies: Vec<String>,
}

/// Discover bounded package and standalone workflow sources using the configured root policy.
///
/// Returns an error when a candidate window would be truncated; callers must not interpret a
/// partial discovery window as a complete catalog.
///
/// Equal-precedence package identities are removed and surfaced as ambiguity diagnostics. Package
/// member files are suppressed from standalone results.
///
/// # Errors
///
/// Returns an error when the workspace cannot be canonicalized or request bounds are invalid.
pub fn discover_workflows(
    workspace: &Path,
    config: &bcode_config::WorkflowsConfig,
    limit: usize,
) -> Result<WorkflowDiscoveryResult, WorkflowDiscoveryError> {
    let mut scan = WorkflowDiscoveryScan::open(workspace, config, limit)?;
    loop {
        if let Some(result) = scan.advance(MAX_DISCOVERY_RESULTS)? {
            return Ok(result);
        }
    }
}

/// Request-local member suppression index, never canonical or reopened after process loss.
/// The private temporary directory is removed after the connection closes.
#[derive(Debug)]
struct PackageMemberIndex {
    connection: rusqlite::Connection,
    directory: tempfile::TempDir,
}

struct IndexedPackage {
    id: String,
    label: String,
    precedence: u32,
    path: PathBuf,
    slot: usize,
}

impl PackageMemberIndex {
    fn new() -> Result<Self, WorkflowDiscoveryError> {
        let directory = tempfile::Builder::new()
            .prefix("bcode-discovery-")
            .tempdir()?;
        let connection = rusqlite::Connection::open(directory.path().join("members.sqlite"))?;
        connection.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; CREATE TABLE members(path BLOB PRIMARY KEY) WITHOUT ROWID; CREATE TABLE ambiguous_packages(id TEXT PRIMARY KEY) WITHOUT ROWID; CREATE TABLE packages(id TEXT PRIMARY KEY, precedence INTEGER NOT NULL, slot INTEGER NOT NULL, metadata TEXT NOT NULL) WITHOUT ROWID;")?;
        Ok(Self {
            connection,
            directory,
        })
    }

    fn insert(&mut self, members: BTreeSet<PathBuf>) -> Result<(), WorkflowDiscoveryError> {
        let transaction = self.connection.transaction()?;
        for member in members {
            transaction.execute(
                "INSERT OR IGNORE INTO members(path) VALUES (?1)",
                [member.as_os_str().as_encoded_bytes()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn store_closure(
        &self,
        slot: usize,
        closure: &bcode_workflow::WorkflowPackageClosure,
    ) -> Result<(), WorkflowDiscoveryError> {
        let path = self.directory.path().join(format!("closure-{slot}.json"));
        let file = fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, closure)?;
        std::io::Write::flush(&mut writer)?;
        Ok(())
    }

    fn load_closure(
        &self,
        slot: usize,
    ) -> Result<bcode_workflow::WorkflowPackageClosure, WorkflowDiscoveryError> {
        let path = self.directory.path().join(format!("closure-{slot}.json"));
        let file = fs::File::open(&path)?;
        let closure = serde_json::from_reader(std::io::BufReader::new(file))?;
        fs::remove_file(path)?;
        Ok(closure)
    }

    fn discard_closure(&self, slot: usize) -> Result<(), WorkflowDiscoveryError> {
        fs::remove_file(self.directory.path().join(format!("closure-{slot}.json")))?;
        Ok(())
    }

    fn package_position(&self, id: &str) -> Result<Option<(u32, usize)>, WorkflowDiscoveryError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .connection
            .query_row(
                "SELECT precedence, slot FROM packages WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    fn store_package(
        &self,
        id: &str,
        label: &str,
        precedence: u32,
        path: &Path,
        slot: usize,
    ) -> Result<(), WorkflowDiscoveryError> {
        // This index is request-local and never reopened on another platform. Serde's
        // native OS-string representation preserves paths that are not Unicode.
        let metadata = serde_json::to_string(&(label, path.as_os_str()))?;
        self.connection.execute("INSERT INTO packages(id, precedence, slot, metadata) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO UPDATE SET precedence=excluded.precedence, slot=excluded.slot, metadata=excluded.metadata", rusqlite::params![id, precedence, slot, metadata])?;
        Ok(())
    }

    fn pop_package(&self) -> Result<Option<IndexedPackage>, WorkflowDiscoveryError> {
        use rusqlite::OptionalExtension;
        let row = self
            .connection
            .query_row(
                "SELECT id, precedence, slot, metadata FROM packages ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, usize>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, precedence, slot, metadata)) = row else {
            return Ok(None);
        };
        let (label, path): (String, std::ffi::OsString) = serde_json::from_str(&metadata)?;
        let path = PathBuf::from(path);
        self.connection
            .execute("DELETE FROM packages WHERE id=?1", [&id])?;
        Ok(Some(IndexedPackage {
            id,
            label,
            precedence,
            path,
            slot,
        }))
    }

    fn mark_ambiguous(&self, id: &str) -> Result<(), WorkflowDiscoveryError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO ambiguous_packages(id) VALUES (?1)",
            [id],
        )?;
        Ok(())
    }

    fn is_ambiguous(&self, id: &str) -> Result<bool, WorkflowDiscoveryError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM ambiguous_packages WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    fn contains(&self, path: &Path) -> Result<bool, WorkflowDiscoveryError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM members WHERE path=?1)",
            [path.as_os_str().as_encoded_bytes()],
            |row| row.get(0),
        )?)
    }
}

/// Process-local discovery and reconciliation across bounded enumeration advances.
///
/// Package roots are reconciled before sources are delivered. Collecting mode retains an
/// aggregate candidate allowance and fails closed on overflow. Streaming mode instead bounds
/// each advance and retains reconciliation in temporary indexed storage. Neither mode provides
/// a durable snapshot or transport cursor.
#[derive(Debug)]
pub struct WorkflowDiscoveryScan {
    roots: Vec<DiscoveryRoot>,
    root_index: usize,
    kind: WorkflowCandidateKind,
    directory: Option<WorkflowDirectoryScan>,
    limit: usize,
    candidates: usize,
    result: WorkflowDiscoveryResult,
    package_members: Option<PackageMemberIndex>,
    terminal: bool,
    streaming: Option<bool>,
}

impl WorkflowDiscoveryScan {
    /// Resolve discovery roots without enumerating their contents.
    ///
    /// # Errors
    /// Returns an error for invalid bounds or an inaccessible workspace.
    pub fn open(
        workspace: &Path,
        config: &bcode_config::WorkflowsConfig,
        limit: usize,
    ) -> Result<Self, WorkflowDiscoveryError> {
        if limit == 0 || limit > MAX_DISCOVERY_RESULTS {
            return Err(WorkflowDiscoveryError::Invalid(format!(
                "workflow discovery limit must be within 1..={MAX_DISCOVERY_RESULTS}"
            )));
        }
        let workspace = fs::canonicalize(workspace)?;
        Ok(Self {
            roots: discovery_roots(&workspace, config),
            root_index: 0,
            kind: WorkflowCandidateKind::Package,
            directory: None,
            limit,
            candidates: 0,
            result: WorkflowDiscoveryResult::default(),
            package_members: Some(PackageMemberIndex::new()?),
            terminal: false,
            streaming: None,
        })
    }

    /// Advance at most `budget` entries or root transitions and return a result only at completion.
    ///
    /// Each candidate uses existing bounded source and package-closure reads. Dropping the scan
    /// releases its handles and accumulated state. Completion consumes the result once; failures
    /// terminate the scan. Callers must restart rather than reuse a completed or failed scan.
    ///
    /// # Errors
    /// Returns an error for invalid budgets, terminal reuse, I/O failures, or candidate overflow.
    pub fn advance(
        &mut self,
        budget: usize,
    ) -> Result<Option<WorkflowDiscoveryResult>, WorkflowDiscoveryError> {
        self.select_delivery(false)?;
        self.advance_checked(budget)
    }

    fn select_delivery(&mut self, streaming: bool) -> Result<(), WorkflowDiscoveryError> {
        if self.streaming.is_some_and(|previous| previous != streaming) {
            return Err(WorkflowDiscoveryError::Invalid(
                "cannot mix discovery delivery modes".into(),
            ));
        }
        self.streaming = Some(streaming);
        Ok(())
    }

    fn advance_checked(
        &mut self,
        budget: usize,
    ) -> Result<Option<WorkflowDiscoveryResult>, WorkflowDiscoveryError> {
        if budget == 0 || budget > MAX_DISCOVERY_RESULTS || self.terminal {
            return Err(WorkflowDiscoveryError::Invalid(
                "invalid discovery advance or terminal scan".into(),
            ));
        }
        self.terminal = true;
        let outcome = self.advance_inner(budget);
        if matches!(&outcome, Ok(None)) {
            self.terminal = false;
        } else {
            self.directory = None;
            self.package_members = None;
            self.result = WorkflowDiscoveryResult::default();
        }
        outcome
    }

    /// Deliver reconciled sources without retaining earlier source payloads.
    ///
    /// Call this method exclusively on a fresh scan; do not mix it with `advance`.
    /// Package sources are delivered only after all package roots have been reconciled.
    /// Dropping the scan releases temporary storage. A later failure invalidates prior batches.
    /// Streaming has no aggregate candidate ceiling: `limit` bounds retained diagnostics,
    /// and each advance's budget bounds delivered sources and enumeration work. Package
    /// reconciliation is request-local indexed storage; consumers must not accumulate batches.
    ///
    /// # Errors
    /// Returns the same budget, terminal, I/O, and overflow errors as `advance`.
    pub fn advance_sources(
        &mut self,
        budget: usize,
    ) -> Result<WorkflowDiscoveryBatch, WorkflowDiscoveryError> {
        self.select_delivery(true)?;
        let result = self.advance_checked(budget)?;
        Ok(match result {
            Some(result) => WorkflowDiscoveryBatch {
                sources: result.sources,
                diagnostics: result.diagnostics,
                complete: true,
            },
            None => WorkflowDiscoveryBatch {
                sources: std::mem::take(&mut self.result.sources),
                diagnostics: Vec::new(),
                complete: false,
            },
        })
    }

    fn advance_inner(
        &mut self,
        budget: usize,
    ) -> Result<Option<WorkflowDiscoveryResult>, WorkflowDiscoveryError> {
        for _ in 0..budget {
            let Some(root) = self.roots.get(self.root_index).cloned() else {
                match self.kind {
                    WorkflowCandidateKind::Package => {
                        // Reconciliation consumes the same budget as enumeration. Do not
                        // drain every retained closure in a single interactive advance.
                        if let Some(IndexedPackage {
                            id: package_id,
                            label: source_label,
                            precedence,
                            path: manifest_path,
                            slot,
                        }) = self
                            .package_members
                            .as_ref()
                            .expect("active index")
                            .pop_package()?
                        {
                            let index = self.package_members.as_ref().expect("active index");
                            if index.is_ambiguous(&package_id)? {
                                index.discard_closure(slot)?;
                            } else {
                                let closure = index.load_closure(slot)?;
                                self.result.sources.push(DiscoveredWorkflowSource::Package {
                                    package_id,
                                    source_label,
                                    precedence,
                                    manifest_path,
                                    closure,
                                });
                            }
                            continue;
                        }
                        self.kind = WorkflowCandidateKind::Standalone;
                        self.root_index = 0;
                        continue;
                    }
                    WorkflowCandidateKind::Standalone => {
                        self.finish()?;
                        return Ok(Some(std::mem::take(&mut self.result)));
                    }
                }
            };
            if self.directory.is_none() {
                self.directory = Some(WorkflowDirectoryScan::open(&root.path, self.kind)?);
            }
            let batch = self
                .directory
                .as_mut()
                .expect("opened directory")
                .advance(1)?;
            for path in batch.paths {
                self.candidates = self.candidates.checked_add(1).ok_or_else(|| {
                    WorkflowDiscoveryError::Invalid(
                        "workflow discovery candidate identity exhausted".into(),
                    )
                })?;
                if self.streaming != Some(true) && self.candidates > self.limit {
                    return Err(WorkflowDiscoveryError::Invalid(
                        "workflow discovery exceeds the aggregate candidate allowance; narrow configured discovery roots".into()
                    ));
                }
                match self.kind {
                    WorkflowCandidateKind::Package => self.package(&root, path)?,
                    WorkflowCandidateKind::Standalone => self.standalone(&root, path)?,
                }
                self.trim_diagnostics();
            }
            if batch.complete {
                self.directory = None;
                self.root_index += 1;
            }
        }
        Ok(None)
    }

    fn package(
        &mut self,
        root: &DiscoveryRoot,
        manifest_path: PathBuf,
    ) -> Result<(), WorkflowDiscoveryError> {
        match read_package_closure(&manifest_path, &root.path) {
            Ok((closure, members)) => {
                self.package_members
                    .as_mut()
                    .expect("active index")
                    .insert(members)?;
                let package_id = closure.entry_package_id.clone();
                match self
                    .package_members
                    .as_ref()
                    .expect("active index")
                    .package_position(&package_id)?
                {
                    Some((precedence, _)) if precedence == root.precedence => {
                        self.package_members
                            .as_ref()
                            .expect("active index")
                            .mark_ambiguous(&package_id)?;
                        self.result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                            source_label: root.label.clone(),
                            path: manifest_path,
                            code: "ambiguous_package_identity".to_string(),
                            message: format!(
                                "package '{package_id}' appears more than once at precedence {}",
                                root.precedence
                            ),
                        });
                    }
                    Some((precedence, _)) if precedence < root.precedence => {}
                    existing => {
                        let slot = existing.map_or(self.candidates, |(_, slot)| slot);
                        self.package_members
                            .as_ref()
                            .expect("active index")
                            .store_closure(slot, &closure)?;
                        self.package_members
                            .as_ref()
                            .expect("active index")
                            .store_package(
                                &package_id,
                                &root.label,
                                root.precedence,
                                &manifest_path,
                                slot,
                            )?;
                    }
                }
            }
            Err(error) => self.result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                source_label: root.label.clone(),
                path: manifest_path,
                code: "invalid_package".to_string(),
                message: error.to_string(),
            }),
        }
        Ok(())
    }

    fn standalone(
        &mut self,
        root: &DiscoveryRoot,
        source_path: PathBuf,
    ) -> Result<(), WorkflowDiscoveryError> {
        let canonical = match fs::canonicalize(&source_path) {
            Ok(path) => path,
            Err(error) => {
                self.result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                    source_label: root.label.clone(),
                    path: source_path,
                    code: "unreadable_source".to_string(),
                    message: error.to_string(),
                });
                return Ok(());
            }
        };
        if self
            .package_members
            .as_ref()
            .expect("active index")
            .contains(&canonical)?
        {
            return Ok(());
        }
        match read_bounded_source(&canonical) {
            Ok(source) => {
                let Some(name) = canonical.file_name().and_then(std::ffi::OsStr::to_str) else {
                    return Ok(());
                };
                match bcode_workflow::WorkflowSourceFormat::from_file_name(name) {
                    Ok(source_format) => {
                        self.result
                            .sources
                            .push(DiscoveredWorkflowSource::Standalone {
                                source_label: root.label.clone(),
                                precedence: root.precedence,
                                source_path: canonical,
                                source_format,
                                source,
                            });
                    }
                    Err(error) => self.result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                        source_label: root.label.clone(),
                        path: canonical,
                        code: "unsupported_source_format".to_string(),
                        message: error.to_string(),
                    }),
                }
            }
            Err(error) => self.result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                source_label: root.label.clone(),
                path: canonical,
                code: "invalid_source".to_string(),
                message: error.to_string(),
            }),
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), WorkflowDiscoveryError> {
        self.result.sources.sort_by(|left, right| {
            (left.precedence(), left.source_key()).cmp(&(right.precedence(), right.source_key()))
        });
        if self.streaming != Some(true) && self.result.sources.len() > self.limit {
            return Err(WorkflowDiscoveryError::Invalid(
            "workflow discovery exceeds the bounded candidate window; narrow configured discovery roots".to_string(),
        ));
        }
        self.trim_diagnostics();
        Ok(())
    }

    fn trim_diagnostics(&mut self) {
        // Stable ordering preserves which equal-key diagnostic wins at the boundary.
        // Each candidate adds at most one entry, so transient retention is limit + 1.
        self.result.diagnostics.sort_by(|left, right| {
            (&left.source_label, &left.path, &left.code).cmp(&(
                &right.source_label,
                &right.path,
                &right.code,
            ))
        });
        self.result.diagnostics.truncate(self.limit);
    }
}

/// Read one explicit package manifest or standalone workflow source outside automatic roots.
///
/// The exact file is canonicalized and treated as its own authorized root boundary. Package member
/// and import paths remain confined beneath the manifest parent.
///
/// # Errors
///
/// Returns an error when the path is missing, unsupported, unconfined, or exceeds source bounds.
pub fn inspect_explicit_source(
    path: &Path,
) -> Result<DiscoveredWorkflowSource, WorkflowDiscoveryError> {
    let path = fs::canonicalize(path)?;
    if !path.is_file() {
        return Err(WorkflowDiscoveryError::Invalid(
            "explicit workflow source is not a file".to_string(),
        ));
    }
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("source name is not UTF-8".to_string()))?;
    if is_package_manifest(name) {
        let root = path.parent().ok_or_else(|| {
            WorkflowDiscoveryError::Invalid("package manifest has no parent".to_string())
        })?;
        let (closure, _) = read_package_closure(&path, root)?;
        return Ok(DiscoveredWorkflowSource::Package {
            package_id: closure.entry_package_id.clone(),
            source_label: "explicit".to_string(),
            precedence: 0,
            manifest_path: path,
            closure,
        });
    }
    if !is_standalone_source(name) {
        return Err(WorkflowDiscoveryError::Invalid(
            "explicit workflow source has an unsupported file name".to_string(),
        ));
    }
    let source_format = bcode_workflow::WorkflowSourceFormat::from_file_name(name)?;
    Ok(DiscoveredWorkflowSource::Standalone {
        source_label: "explicit".to_string(),
        precedence: 0,
        source_path: path.clone(),
        source_format,
        source: read_bounded_source(&path)?,
    })
}

fn discovery_roots(workspace: &Path, config: &bcode_config::WorkflowsConfig) -> Vec<DiscoveryRoot> {
    let mut roots = Vec::new();
    if config.include_repo_workflows {
        roots.extend([
            DiscoveryRoot {
                path: workspace.join(".bcode/workflows"),
                label: "repository:.bcode/workflows".to_string(),
                precedence: 10,
            },
            DiscoveryRoot {
                path: workspace.join("workflows"),
                label: "repository:workflows".to_string(),
                precedence: 20,
            },
        ]);
    }
    roots.extend(
        config
            .paths
            .iter()
            .enumerate()
            .map(|(index, path)| DiscoveryRoot {
                path: path.clone(),
                label: "configured".to_string(),
                precedence: 30_u32.saturating_add(u32::try_from(index).unwrap_or(u32::MAX - 30)),
            }),
    );
    if config.include_user_workflows {
        roots.extend([
            DiscoveryRoot {
                path: bcode_config::default_config_dir().join("workflows"),
                label: "user-config:workflows".to_string(),
                precedence: 100,
            },
            DiscoveryRoot {
                path: bcode_config::default_state_dir().join("workflows"),
                label: "user-state:workflows".to_string(),
                precedence: 110,
            },
        ]);
    }
    roots
}

/// Candidate classification for an incremental workflow directory scan.
#[derive(Debug, Clone, Copy)]
pub enum WorkflowCandidateKind {
    /// Package manifests, reconciled before standalone sources.
    Package,
    /// Standalone source candidates, before package-member suppression.
    Standalone,
}

/// One bounded scan advance. Candidates are not yet a reconciled launch catalog.
#[derive(Debug)]
pub struct WorkflowDirectoryBatch {
    /// Matching regular files, in filesystem enumeration order.
    pub paths: Vec<PathBuf>,
    /// All directory entries consumed, including nonmatching entries.
    pub inspected: usize,
    /// Whether enumeration has reached its end.
    pub complete: bool,
}

/// Incremental, process-local enumeration of one workflow discovery directory.
///
/// Dropping the scan releases its directory handle. Advances do not rescan earlier entries.
/// This is not a filesystem snapshot: callers must reconcile candidates and establish catalog
/// validity before exposing ordered pages. The handle is not a durable or public transport cursor.
#[derive(Debug)]
pub struct WorkflowDirectoryScan {
    entries: Option<fs::ReadDir>,
    root: PathBuf,
    modified: Option<std::time::SystemTime>,
    kind: WorkflowCandidateKind,
    failed: bool,
}

impl WorkflowDirectoryScan {
    /// Open a directory scan. A missing optional discovery root produces an empty scan.
    ///
    /// # Errors
    /// Returns I/O errors for inaccessible roots or roots that are not directories.
    pub fn open(root: &Path, kind: WorkflowCandidateKind) -> Result<Self, std::io::Error> {
        let entries = match fs::read_dir(root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let modified = if entries.is_some() {
            Some(fs::metadata(root)?.modified()?)
        } else {
            None
        };
        Ok(Self {
            entries,
            root: root.to_path_buf(),
            modified,
            kind,
            failed: false,
        })
    }

    /// Consume at most `budget` directory entries, retaining position for the next advance.
    ///
    /// An exact-budget final batch can require another advance to observe completion. Errors
    /// terminate this scan; callers must discard earlier batches rather than infer completeness.
    ///
    /// # Errors
    /// Returns an error for a zero or excessive budget, enumeration failures, or unreadable
    /// matching candidate metadata. Symlink targets are followed, not authorized by this scan.
    pub fn advance(&mut self, budget: usize) -> Result<WorkflowDirectoryBatch, std::io::Error> {
        if budget == 0 || budget > MAX_DISCOVERY_RESULTS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workflow directory scan budget is outside supported bounds",
            ));
        }
        if self.failed {
            return Err(std::io::Error::other(
                "workflow directory scan previously failed; restart discovery",
            ));
        }
        let mut batch = WorkflowDirectoryBatch {
            paths: Vec::new(),
            inspected: 0,
            complete: self.entries.is_none(),
        };
        // Taking the handle makes any I/O failure terminal instead of allowing skipped entries.
        let Some(mut entries) = self.entries.take() else {
            return Ok(batch);
        };
        self.failed = true;
        self.checkdirectory_stamp()?;
        for _ in 0..budget {
            let Some(entry) = entries.next() else {
                self.checkdirectory_stamp()?;
                self.failed = false;
                batch.complete = true;
                return Ok(batch);
            };
            let path = entry?.path();
            batch.inspected += 1;
            let predicate = match self.kind {
                WorkflowCandidateKind::Package => is_package_manifest,
                WorkflowCandidateKind::Standalone => is_standalone_source,
            };
            if path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(predicate)
                && fs::metadata(&path)?.is_file()
            {
                batch.paths.push(path);
            }
        }
        self.checkdirectory_stamp()?;
        self.failed = false;
        self.entries = Some(entries);
        Ok(batch)
    }

    fn checkdirectory_stamp(&self) -> Result<(), std::io::Error> {
        if Some(fs::metadata(&self.root)?.modified()?) != self.modified {
            return Err(std::io::Error::other(
                "workflow discovery directory changed during enumeration; restart discovery",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
fn matching_files(
    root: &Path,
    kind: WorkflowCandidateKind,
    limit: usize,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut scan = WorkflowDirectoryScan::open(root, kind)?;
    let mut paths = Vec::new();
    loop {
        let batch = scan.advance(MAX_DISCOVERY_RESULTS)?;
        for path in batch.paths {
            paths.push(path);
            if paths.len() > limit {
                return Err(std::io::Error::other(
                    "workflow discovery exceeds the bounded candidate window; narrow configured discovery roots",
                ));
            }
        }
        if batch.complete {
            break;
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_package_manifest(name: &str) -> bool {
    [
        ".workflow-package.json",
        ".workflow-package.yaml",
        ".workflow-package.yml",
        ".workflow-package.toml",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

fn is_standalone_source(name: &str) -> bool {
    !is_package_manifest(name)
        && [
            ".workflow.json",
            ".workflow.yaml",
            ".workflow.yml",
            ".workflow.toml",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn read_package_closure(
    entry: &Path,
    authorized_root: &Path,
) -> Result<(bcode_workflow::WorkflowPackageClosure, BTreeSet<PathBuf>), WorkflowDiscoveryError> {
    let entry = fs::canonicalize(entry)?;
    let authorized_root = fs::canonicalize(authorized_root)?;
    if !entry.starts_with(&authorized_root) {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest escapes its authorized root".to_string(),
        ));
    }
    let mut pending = vec![(entry, 1_usize)];
    let mut visited = BTreeSet::new();
    let mut packages = Vec::new();
    let mut members = BTreeSet::new();
    while let Some((manifest_path, depth)) = pending.pop() {
        if depth > bcode_workflow::MAX_WORKFLOW_PACKAGE_DEPTH {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package import depth exceeds the package bound".to_string(),
            ));
        }
        if !visited.insert(manifest_path.clone()) {
            continue;
        }
        if packages.len() >= bcode_workflow::MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package closure exceeds the package-count bound".to_string(),
            ));
        }
        let (manifest, manifest_members, imports) =
            read_package_manifest(&manifest_path, &authorized_root)?;
        members.extend(manifest_members);
        pending.extend(imports.into_iter().rev().map(|path| (path, depth + 1)));
        packages.push(bcode_workflow::WorkflowPackageClosureSource {
            package_id: manifest.package_id.clone(),
            source_name: Some(manifest_path.display().to_string()),
            manifest,
        });
    }
    let entry_package_id = packages
        .first()
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("empty package closure".to_string()))?
        .package_id
        .clone();
    Ok((
        bcode_workflow::WorkflowPackageClosure {
            version: bcode_workflow::WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id,
            packages,
        },
        members,
    ))
}

fn read_package_manifest(
    path: &Path,
    authorized_root: &Path,
) -> Result<
    (
        bcode_workflow::WorkflowPackageManifest,
        BTreeSet<PathBuf>,
        Vec<PathBuf>,
    ),
    WorkflowDiscoveryError,
> {
    let manifest_path = fs::canonicalize(path)?;
    let package_root = manifest_path.parent().ok_or_else(|| {
        WorkflowDiscoveryError::Invalid("workflow package manifest has no parent".to_string())
    })?;
    if !manifest_path.starts_with(authorized_root) {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest escapes its authorized root".to_string(),
        ));
    }
    let manifest_source = read_bounded_package_source(&manifest_path)?;
    let name = manifest_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("manifest name is not UTF-8".to_string()))?;
    let decoded: SourcePackageManifest =
        match bcode_workflow::WorkflowSourceFormat::from_file_name(name)? {
            bcode_workflow::WorkflowSourceFormat::Json => serde_json::from_str(&manifest_source)?,
            bcode_workflow::WorkflowSourceFormat::Yaml => yaml_serde::from_str(&manifest_source)
                .map_err(|error| {
                    WorkflowDiscoveryError::Invalid(format!("invalid YAML manifest: {error}"))
                })?,
            bcode_workflow::WorkflowSourceFormat::Toml => toml::from_str(&manifest_source)?,
        };
    let mut member_paths = BTreeSet::new();
    let mut total_bytes = manifest_source.len();
    let mut manifest = bcode_workflow::WorkflowPackageManifest {
        version: decoded.version,
        package_id: decoded.package_id,
        exports: decoded.exports,
        external_dependencies: decoded.external_dependencies,
        imports: decoded
            .imports
            .iter()
            .map(|import| bcode_workflow::WorkflowPackageImport {
                import_id: import.import_id.clone(),
                package_id: import.package_id.clone(),
                export: import.export.clone(),
                manifest: import.manifest.clone(),
                target: import.target.clone(),
                package_lock_digest_sha256: import.package_lock_digest_sha256.clone(),
            })
            .collect(),
        members: decoded
            .members
            .into_iter()
            .map(|member| bcode_workflow::WorkflowPackageMember {
                member_id: member.member_id,
                source_name: member.source_name,
                format: bcode_workflow::WorkflowSourceFormat::Json,
                source: String::new(),
                dependencies: member.dependencies,
                external_dependencies: member.external_dependencies,
            })
            .collect(),
    };
    for member in &mut manifest.members {
        let relative = confined_relative_path(&member.source_name)?;
        let path = fs::canonicalize(package_root.join(relative))?;
        if !path.starts_with(package_root) || !path.is_file() {
            return Err(WorkflowDiscoveryError::Invalid(format!(
                "workflow package member '{}' escapes its package root or is not a file",
                member.source_name
            )));
        }
        let source = fs::read_to_string(&path)?;
        total_bytes = total_bytes.checked_add(source.len()).ok_or_else(|| {
            WorkflowDiscoveryError::Invalid("workflow package byte count overflow".to_string())
        })?;
        if total_bytes > bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package sources exceed the package byte bound".to_string(),
            ));
        }
        member.format = bcode_workflow::WorkflowSourceFormat::from_file_name(&member.source_name)?;
        member.source = source;
        member_paths.insert(path);
    }
    manifest.validate()?;
    let mut imports = Vec::new();
    for import in &decoded.imports {
        if let Some(relative) = &import.manifest {
            let path = fs::canonicalize(package_root.join(confined_relative_path(relative)?))?;
            if !path.starts_with(authorized_root) || !path.is_file() {
                return Err(WorkflowDiscoveryError::Invalid(format!(
                    "workflow package import '{relative}' escapes its authorized root or is not a file"
                )));
            }
            imports.push(path);
        }
    }
    imports.sort();
    Ok((manifest, member_paths, imports))
}

fn confined_relative_path(value: &str) -> Result<&Path, WorkflowDiscoveryError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WorkflowDiscoveryError::Invalid(format!(
            "workflow path '{value}' is not confined"
        )));
    }
    Ok(path)
}

fn read_source_window(path: &Path, limit: usize) -> Result<String, std::io::Error> {
    use std::io::Read as _;
    let mut source = String::new();
    fs::File::open(path)?
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_string(&mut source)?;
    Ok(source)
}

fn read_bounded_package_source(path: &Path) -> Result<String, WorkflowDiscoveryError> {
    let source = read_source_window(path, bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES)?;
    if source.len() > bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest exceeds the package byte bound".to_string(),
        ));
    }
    Ok(source)
}

fn read_bounded_source(path: &Path) -> Result<String, WorkflowDiscoveryError> {
    let source = read_source_window(path, bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES)?;
    if source.len() > bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow source exceeds the authoring byte bound".to_string(),
        ));
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_crosses_candidate_ceiling_without_retaining_sources() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("workflows");
        std::fs::create_dir(&directory).unwrap();
        let count = super::MAX_DISCOVERY_RESULTS + 3;
        for index in 0..count {
            std::fs::write(directory.join(format!("{index}.workflow.json")), "{}").unwrap();
        }
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: true,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        let mut scan = super::WorkflowDiscoveryScan::open(root.path(), &config, 1).unwrap();
        let mut delivered = 0;
        loop {
            let batch = scan.advance_sources(16).unwrap();
            assert!(batch.sources.len() <= 16);
            assert!(scan.result.sources.is_empty());
            assert!(scan.result.diagnostics.len() <= 1);
            delivered += batch.sources.len();
            if batch.complete {
                break;
            }
        }
        assert_eq!(delivered, count);
        assert!(super::discover_workflows(root.path(), &config, 1).is_err());
    }

    #[test]
    fn diagnostic_window_matches_stable_full_projection() {
        let root = tempfile::tempdir().unwrap();
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: false,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        let mut scan = super::WorkflowDiscoveryScan::open(root.path(), &config, 2).unwrap();
        let mut all = Vec::new();
        for (index, label) in ["z", "b", "b", "a", "b"].into_iter().enumerate() {
            let diagnostic = super::WorkflowDiscoveryDiagnostic {
                source_label: label.into(),
                path: "source".into(),
                code: "invalid".into(),
                message: index.to_string(),
            };
            all.push(diagnostic.clone());
            scan.result.diagnostics.push(diagnostic);
            scan.trim_diagnostics();
            assert!(scan.result.diagnostics.len() <= 2);
        }
        all.sort_by(|left, right| {
            (&left.source_label, &left.path, &left.code).cmp(&(
                &right.source_label,
                &right.path,
                &right.code,
            ))
        });
        all.truncate(2);
        assert_eq!(scan.result.diagnostics, all);
    }
    #[cfg(unix)]
    #[test]
    fn indexed_metadata_preserves_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt;
        let index = super::PackageMemberIndex::new().unwrap();
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
            b"/workflow-\xff/manifest.json".to_vec(),
        ));
        index
            .store_package("package", "repository", 10, &path, 7)
            .unwrap();
        assert_eq!(index.package_position("package").unwrap(), Some((10, 7)));
        let package = index.pop_package().unwrap().unwrap();
        assert_eq!(package.path, path);
        assert_eq!(package.label, "repository");
        assert!(index.pop_package().unwrap().is_none());
    }
    #[test]
    fn changeddirectory_stamp_invalidates_scan_permanently() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("one.workflow.json"), "{}").unwrap();
        let mut scan = super::WorkflowDirectoryScan::open(
            root.path(),
            super::WorkflowCandidateKind::Standalone,
        )
        .unwrap();
        assert!(!scan.advance(1).unwrap().complete);
        // Inject an old stamp deterministically, independent of filesystem timestamp resolution.
        scan.modified = Some(std::time::UNIX_EPOCH);
        assert!(scan.advance(1).unwrap_err().to_string().contains("changed"));
        assert!(scan.entries.is_none());
        assert!(
            scan.advance(1)
                .unwrap_err()
                .to_string()
                .contains("previously failed")
        );
    }

    #[test]
    fn discovery_scan_yields_and_delivers_result_once() {
        let root = tempfile::tempdir().unwrap();
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: true,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        let mut scan = super::WorkflowDiscoveryScan::open(root.path(), &config, 10).unwrap();
        assert!(scan.advance(1).unwrap().is_none());
        let mut completed = false;
        for _ in 0..100 {
            if let Some(result) = scan.advance(1).unwrap() {
                assert!(result.sources.is_empty());
                completed = true;
                break;
            }
        }
        assert!(completed);
        assert!(scan.advance(1).is_err());
    }

    #[test]
    fn source_delivery_drains_payloads_and_rejects_mode_switching() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("workflows");
        std::fs::create_dir(&directory).unwrap();
        for name in ["a.workflow.json", "b.workflow.json"] {
            std::fs::write(directory.join(name), "{}").unwrap();
        }
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: true,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        let mut scan = super::WorkflowDiscoveryScan::open(root.path(), &config, 10).unwrap();
        let mut delivered = Vec::new();
        loop {
            let batch = scan.advance_sources(1).unwrap();
            assert!(batch.sources.len() <= 1);
            assert!(scan.result.sources.is_empty());
            delivered.extend(batch.sources);
            if batch.complete {
                break;
            }
            assert!(scan.advance(1).is_err());
        }
        assert_eq!(delivered.len(), 2);
        assert!(scan.advance_sources(1).is_err());
    }

    #[test]
    fn source_window_reads_only_limit_plus_one_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.json");
        std::fs::write(&path, "abcdefghijklmnop").unwrap();
        assert_eq!(super::read_source_window(&path, 4).unwrap(), "abcde");
        assert_eq!(
            super::read_source_window(&path, 16).unwrap(),
            "abcdefghijklmnop"
        );
    }

    #[test]
    fn incremental_scan_bounds_all_entries_and_continues_beyond_candidate_window() {
        let root = tempfile::tempdir().unwrap();
        let count = super::MAX_DISCOVERY_RESULTS + 3;
        for index in 0..count {
            std::fs::write(root.path().join(format!("{index}.workflow.json")), "{}").unwrap();
            std::fs::write(root.path().join(format!("{index}.txt")), "ignored").unwrap();
        }
        let mut scan = super::WorkflowDirectoryScan::open(
            root.path(),
            super::WorkflowCandidateKind::Standalone,
        )
        .unwrap();
        assert!(scan.advance(0).is_err());
        let mut paths = std::collections::BTreeSet::new();
        let mut inspected = 0;
        loop {
            let batch = scan.advance(7).unwrap();
            assert!(batch.inspected <= 7);
            assert!(batch.paths.len() <= batch.inspected);
            inspected += batch.inspected;
            for path in batch.paths {
                assert!(paths.insert(path));
            }
            if batch.complete {
                break;
            }
        }
        assert_eq!(paths.len(), count);
        assert_eq!(inspected, count * 2);
        let done = scan.advance(7).unwrap();
        assert!(done.complete);
        assert_eq!(done.inspected, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_candidate_is_not_silently_omitted() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            root.path().join("missing"),
            root.path().join("broken.workflow.json"),
        )
        .unwrap();
        let error =
            super::matching_files(root.path(), super::WorkflowCandidateKind::Standalone, 10)
                .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn discovery_rejects_truncated_candidate_windows() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a.workflow.json", "b.workflow.json", "c.workflow.json"] {
            std::fs::write(root.path().join(name), "{}").unwrap();
        }
        let error = super::matching_files(root.path(), super::WorkflowCandidateKind::Standalone, 2)
            .unwrap_err();
        assert!(error.to_string().contains("bounded candidate window"));
        assert_eq!(
            super::matching_files(root.path(), super::WorkflowCandidateKind::Standalone, 3)
                .unwrap()
                .len(),
            3
        );
    }

    use super::*;

    fn source(workflow_id: &str, title: &str) -> String {
        format!(
            r"workflow_source_version: 3
workflow_id: {workflow_id}
title: {title}
steps:
  - id: done
    name: Done
    output:
      type_name: example/output
      schema: {{}}
"
        )
    }

    #[test]
    fn discovers_standalone_sources_and_suppresses_package_members() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join(".bcode/workflows");
        fs::create_dir_all(&root).expect("root");
        fs::write(
            root.join("standalone.workflow.yaml"),
            source("standalone", "Standalone"),
        )
        .expect("standalone");
        fs::write(
            root.join("member.workflow.yaml"),
            source("member", "Member"),
        )
        .expect("member");
        fs::write(
            root.join("example.workflow-package.yaml"),
            r"version: 3
package_id: example/package
exports: { main: member }
members:
  - member_id: member
    source_name: member.workflow.yaml
",
        )
        .expect("manifest");

        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: true,
                include_user_workflows: false,
                paths: Vec::new(),
                ..bcode_config::WorkflowsConfig::default()
            },
            20,
        )
        .expect("discovery");

        assert_eq!(result.sources.len(), 2);
        assert!(result.sources.iter().any(|source| matches!(
            source,
            DiscoveredWorkflowSource::Package { package_id, .. }
                if package_id == "example/package"
        )));
        assert!(result.sources.iter().any(|source| matches!(
            source,
            DiscoveredWorkflowSource::Standalone { source_path, .. }
                if source_path.ends_with("standalone.workflow.yaml")
        )));
    }

    #[test]
    fn mixed_streaming_matches_collection_and_releases_temporary_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workflows");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("member.workflow.yaml"),
            source("member", "Member"),
        )
        .unwrap();
        fs::write(
            root.join("standalone.workflow.yaml"),
            source("standalone", "Standalone"),
        )
        .unwrap();
        fs::write(root.join("example.workflow-package.yaml"), "version: 3\npackage_id: example/package\nexports: { main: member }\nmembers:\n  - member_id: member\n    source_name: member.workflow.yaml\n").unwrap();
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: true,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        let expected = discover_workflows(temp.path(), &config, 20).unwrap();
        for budget in [1, 2, 7] {
            let mut scan = WorkflowDiscoveryScan::open(temp.path(), &config, 20).unwrap();
            let temporary = scan
                .package_members
                .as_ref()
                .unwrap()
                .directory
                .path()
                .to_path_buf();
            let mut actual = WorkflowDiscoveryResult::default();
            loop {
                let batch = scan.advance_sources(budget).unwrap();
                assert!(batch.sources.len() <= budget);
                actual.sources.extend(batch.sources);
                if batch.complete {
                    actual.diagnostics = batch.diagnostics;
                    break;
                }
            }
            actual
                .sources
                .sort_by_key(|source| (source.precedence(), source.source_key()));
            assert_eq!(actual, expected);
            assert!(!temporary.exists());
        }
        let mut scan = WorkflowDiscoveryScan::open(temp.path(), &config, 20).unwrap();
        let temporary = scan
            .package_members
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_path_buf();
        assert!(!scan.advance_sources(1).unwrap().complete);
        drop(scan);
        assert!(!temporary.exists());
    }

    #[test]
    fn package_streaming_crosses_ceiling_and_releases_spools() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workflows");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("member.workflow.yaml"),
            source("member", "Member"),
        )
        .unwrap();
        let count = super::MAX_DISCOVERY_RESULTS + 3;
        for index in 0..count {
            fs::write(root.join(format!("{index}.workflow-package.yaml")), format!("version: 3\npackage_id: example/package-{index:04}\nexports: {{ main: member }}\nmembers:\n  - member_id: member\n    source_name: member.workflow.yaml\n")).unwrap();
        }
        let config = bcode_config::WorkflowsConfig {
            include_repo_workflows: true,
            include_user_workflows: false,
            paths: Vec::new(),
            ..Default::default()
        };
        for complete in [false, true] {
            let mut scan = WorkflowDiscoveryScan::open(temp.path(), &config, 1).unwrap();
            let temporary = scan
                .package_members
                .as_ref()
                .unwrap()
                .directory
                .path()
                .to_path_buf();
            let mut delivered = 0;
            loop {
                let batch = scan.advance_sources(16).unwrap();
                assert!(batch.sources.len() <= 16);
                assert!(scan.result.sources.is_empty());
                assert!(batch.diagnostics.is_empty());
                for source in batch.sources {
                    assert!(matches!(
                        source,
                        super::DiscoveredWorkflowSource::Package { .. }
                    ));
                    delivered += 1;
                }
                if batch.complete {
                    assert!(complete);
                    assert_eq!(delivered, count);
                    assert!(!temporary.exists());
                    break;
                }
                if !complete && delivered > 0 {
                    assert!(temporary.exists());
                    break;
                }
            }
            drop(scan);
            assert!(!temporary.exists());
        }
    }

    #[test]
    fn equal_precedence_duplicate_packages_fail_closed_as_ambiguous() {
        let temp = tempfile::tempdir().expect("temp");
        let configured = temp.path().join("configured");
        fs::create_dir_all(&configured).expect("configured");
        for suffix in ["a", "b"] {
            fs::write(
                configured.join(format!("{suffix}.workflow.yaml")),
                source(suffix, suffix),
            )
            .expect("source");
            fs::write(
                configured.join(format!("{suffix}.workflow-package.yaml")),
                format!(
                    "version: 3\npackage_id: duplicate/package\nexports: {{ main: {suffix} }}\nmembers:\n  - member_id: {suffix}\n    source_name: {suffix}.workflow.yaml\n"
                ),
            )
            .expect("manifest");
        }
        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: false,
                include_user_workflows: false,
                paths: vec![configured],
                ..bcode_config::WorkflowsConfig::default()
            },
            20,
        )
        .expect("discovery");
        assert!(result.sources.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "ambiguous_package_identity")
        );
    }

    #[test]
    fn rejects_parent_traversal_in_package_members() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workflows");
        fs::create_dir_all(&root).expect("root");
        fs::write(
            root.join("bad.workflow-package.yaml"),
            "version: 3\npackage_id: bad/package\nexports: { main: bad }\nmembers:\n  - member_id: bad\n    source_name: ../bad.workflow.yaml\n",
        )
        .expect("manifest");
        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: true,
                include_user_workflows: false,
                paths: Vec::new(),
                ..bcode_config::WorkflowsConfig::default()
            },
            20,
        )
        .expect("discovery");
        assert!(result.sources.is_empty());
        assert_eq!(result.diagnostics[0].code, "invalid_package");
    }

    #[test]
    fn explicit_source_inspection_is_confined_to_the_exact_file() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("outside.workflow.yaml");
        fs::write(&path, source("outside", "Outside")).expect("source");
        let discovered = inspect_explicit_source(&path).expect("explicit source");
        assert!(matches!(
            discovered,
            DiscoveredWorkflowSource::Standalone {
                source_label,
                source_path,
                source_format: bcode_workflow::WorkflowSourceFormat::Yaml,
                ..
            } if source_label == "explicit" && source_path == fs::canonicalize(path).expect("path")
        ));
    }
}
