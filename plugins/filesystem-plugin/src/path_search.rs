//! Path-only composer search over an invocation-authorized workspace.
//!
//! `bcode.filesystem/v1`, operation `search_paths`, uses the JSON types below.
//! Authorization is NOT part of the caller's payload: the invocation-bound host
//! bridge must implement `bcode.workspace-access/v1/authorize`.
//! It must verify the invocation identity, derive canonical workspace context,
//! evaluate canonical enumeration/read policy facts, and return `SearchPathsGrant`
//! only after authorization. Unsupported/denied/missing bridges fail closed.
//! Only separately authorized, bounded ignore/Git metadata reads are performed.
//! No content is returned or attached. Unsupported ignore syntax fails closed.

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod git_config;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod git_policy;
mod ignore_policy;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod inventory;
mod matcher;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod prepared;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod workspace_inventory;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
use std::path::PathBuf;
use std::path::{Component, Path};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
use std::time::{Duration, Instant};

use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::workspace_access::{
    ExternalResourceRequest, OP_EXTERNAL_RESOURCE_AUTHORIZE, OP_WORKSPACE_ACCESS_AUTHORIZE,
    OP_WORKSPACE_RESOURCE_AUTHORIZE, WORKSPACE_ACCESS_INTERFACE_ID,
    WorkspaceAccessGrant as SearchPathsGrant, WorkspaceAccessRequest, WorkspaceResourceGrant,
    WorkspaceResourceRequest,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use serde::{Deserialize, Serialize};

/// Filesystem wire operation, additive to the existing v1 service.
pub const OP_SEARCH_PATHS: &str = "search_paths";
/// Invocation-bound host authorization interface, version 1.
pub const PATH_SEARCH_AUTHORIZATION_INTERFACE: &str = WORKSPACE_ACCESS_INTERFACE_ID;

/// Path search request. Unknown fields are rejected; roots/grants are never accepted here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchPathsRequest {
    /// Case-insensitive Unicode subsequence matched against workspace-relative paths.
    pub query: String,
    /// Git-index tracked paths by default; workspace also includes untracked paths.
    pub scope: SearchPathsScope,
    /// Bypass a fresh inventory and coalesce a bounded refresh.
    pub refresh: bool,
    /// Workspace-relative directory; empty means workspace root. No `..` or absolute paths.
    pub directory: String,
    /// Recurse beneath `directory` when true; false returns its immediate children.
    pub recursive: bool,
    /// Include dot-prefixed names, except `.git`, when host policy permits it.
    pub include_hidden: bool,
    /// Result bound, clamped to `1..=256`.
    pub max_results: usize,
    /// Enumeration bound, clamped to `1..=250_000`, including excluded names.
    pub max_entries: usize,
    /// Directory descent bound, clamped to `1..=64`.
    pub max_depth: usize,
    /// Cooperative traversal deadline, clamped to `1..=1000` milliseconds.
    pub timeout_ms: u64,
    /// Cumulative path-storage/output budget, clamped to `1024..=1_048_576` bytes.
    pub max_bytes: usize,
}

impl Default for SearchPathsRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: SearchPathsScope::Tracked,
            refresh: false,
            directory: String::new(),
            recursive: true,
            include_hidden: false,
            max_results: 100,
            max_entries: 250_000,
            max_depth: 32,
            timeout_ms: 100,
            max_bytes: 262_144,
        }
    }
}

/// Truthful traversal outcome; empty complete results differ from unavailable search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathsState {
    /// All permitted entries in the requested scope were examined.
    Complete,
    /// A budget, depth boundary, encoding limitation, or inaccessible entry stopped work.
    Incomplete,
    /// Invocation cancellation was observed.
    Cancelled,
    /// Context/authorization/platform/root was unavailable. No results are returned.
    Unavailable,
}

/// Kind of an insertion candidate. Symlinks and special files are never candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathKind {
    /// Regular file (contents are not opened).
    File,
    /// Directory (consumer may navigate using this relative path).
    Directory,
}

/// Requested/effective path inventory scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPathsScope {
    /// Existing Git-index files, including staged additions.
    #[default]
    Tracked,
    /// Tracked and nonignored untracked files; nonrepo confined traversal.
    Workspace,
}

/// Half-open byte range in the original UTF-8 relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPathMatchRange {
    /// Inclusive byte offset.
    pub start: usize,
    /// Exclusive byte offset.
    pub end: usize,
}

/// Workspace-relative candidate, not a file attachment or a content reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPathEntry {
    /// Exact UTF-8 workspace-relative path; spaces and Unicode are preserved.
    pub path: String,
    /// File versus navigable directory.
    pub kind: SearchPathKind,
    /// Lower is better. Equal scores are ordered by exact relative path.
    pub score: usize,
    /// Matching original-path byte ranges, empty for an empty query.
    #[serde(default)]
    pub match_ranges: Vec<SearchPathMatchRange>,
}

/// Bounded path-only response; consumers must display non-complete states explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPathsResponse {
    /// Candidates in deterministic score/path order within the examined scope.
    pub entries: Vec<SearchPathEntry>,
    /// Whether results exhaust the requested authorized scope.
    pub state: SearchPathsState,
    /// Stable reason code, never an OS error containing sensitive paths.
    pub reason: Option<String>,
    /// Number of directory entries examined, including hidden/ignored entries.
    pub visited_entries: usize,
    /// Scope actually searched; nonrepo fallback requires explicit workspace scope.
    #[serde(default)]
    pub effective_scope: SearchPathsScope,
}

impl SearchPathsResponse {
    fn terminal(state: SearchPathsState, reason: &str) -> Self {
        Self {
            entries: Vec::new(),
            state,
            reason: Some(reason.into()),
            visited_entries: 0,
            effective_scope: SearchPathsScope::Tracked,
        }
    }
}

pub(crate) fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    let response = invoke_response(context);
    super::json_response(&response)
}

fn invoke_response(context: &NativeServiceContext) -> SearchPathsResponse {
    use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
    if context.cancellation.is_cancelled() {
        return SearchPathsResponse::terminal(SearchPathsState::Cancelled, "cancelled");
    }
    if context.request.payload.len() > 16_384 {
        return SearchPathsResponse::terminal(SearchPathsState::Unavailable, "request_too_large");
    }
    let Ok(request) = context.request.payload_json::<SearchPathsRequest>() else {
        return SearchPathsResponse::terminal(SearchPathsState::Unavailable, "invalid_request");
    };
    if !valid_request(&request) {
        return SearchPathsResponse::terminal(SearchPathsState::Unavailable, "invalid_request");
    }
    let authorization = WorkspaceAccessRequest {
        directory: request.directory.clone(),
        recursive: request.recursive,
        include_hidden: request.include_hidden,
    };
    let bridge_request = ServiceBridgeRequest::InvokeService(ToolInvocationServiceRequest {
        invocation_id: String::new(),
        request_id: "filesystem-search-paths-authorization".into(),
        route_id: None,
        interface_id: PATH_SEARCH_AUTHORIZATION_INTERFACE.into(),
        operation: OP_WORKSPACE_ACCESS_AUTHORIZE.into(),
        payload: serde_json::json!(authorization),
    });
    let grant = match context.bridge.request(&bridge_request) {
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
            payload,
        })) => match serde_json::from_value::<SearchPathsGrant>(payload) {
            Ok(grant) => grant,
            Err(_) => {
                return SearchPathsResponse::terminal(
                    SearchPathsState::Unavailable,
                    "invalid_grant",
                );
            }
        },
        Ok(ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled)) => {
            return SearchPathsResponse::terminal(SearchPathsState::Cancelled, "cancelled");
        }
        _ => {
            return SearchPathsResponse::terminal(
                if context.cancellation.is_cancelled() {
                    SearchPathsState::Cancelled
                } else {
                    SearchPathsState::Unavailable
                },
                "authorization_unavailable",
            );
        }
    };
    search_with_metadata(
        &request,
        &grant,
        &|| context.cancellation.is_cancelled(),
        &|path| {
            let request = WorkspaceResourceRequest { path: path.into() };
            if !request.is_normalized() {
                return None;
            }
            let nested = ServiceBridgeRequest::InvokeService(ToolInvocationServiceRequest {
                invocation_id: String::new(),
                request_id: format!("filesystem-ignore:{path}"),
                route_id: None,
                interface_id: WORKSPACE_ACCESS_INTERFACE_ID.into(),
                operation: OP_WORKSPACE_RESOURCE_AUTHORIZE.into(),
                payload: serde_json::json!(request),
            });
            match context.bridge.request(&nested) {
                Ok(ServiceBridgeResponse::Service(
                    ToolInvocationServiceResolution::Responded { payload },
                )) => serde_json::from_value::<WorkspaceResourceGrant>(payload).ok(),
                _ => None,
            }
        },
        &|path| {
            let nested = ServiceBridgeRequest::InvokeService(ToolInvocationServiceRequest {
                invocation_id: String::new(),
                request_id: "filesystem-git-metadata-authorization".into(),
                route_id: None,
                interface_id: WORKSPACE_ACCESS_INTERFACE_ID.into(),
                operation: OP_EXTERNAL_RESOURCE_AUTHORIZE.into(),
                payload: serde_json::json!(ExternalResourceRequest {
                    path: path.to_owned()
                }),
            });
            match context.bridge.request(&nested) {
                Ok(ServiceBridgeResponse::Service(
                    ToolInvocationServiceResolution::Responded { payload },
                )) => serde_json::from_value::<WorkspaceResourceGrant>(payload).ok(),
                _ => None,
            }
        },
    )
}

fn valid_relative(path: &Path) -> bool {
    path.to_str().is_some_and(|directory| {
        WorkspaceAccessRequest {
            directory: directory.into(),
            recursive: false,
            include_hidden: false,
        }
        .is_normalized()
    })
}

fn valid_request(request: &SearchPathsRequest) -> bool {
    request.query.len() <= 1024
        && WorkspaceAccessRequest {
            directory: request.directory.clone(),
            recursive: request.recursive,
            include_hidden: request.include_hidden,
        }
        .is_normalized()
        && Path::new(&request.directory).components().count() <= 64
}

fn excluded(path: &Path, grant: &SearchPathsGrant, hidden: bool) -> bool {
    path.components().any(|part| {
        let Component::Normal(name) = part else {
            return true;
        };
        name == ".git" || (!hidden && name.to_str().is_none_or(|name| name.starts_with('.')))
    }) || grant
        .excluded_prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn search_with_metadata(
    _: &SearchPathsRequest,
    _: &SearchPathsGrant,
    _: &dyn Fn() -> bool,
    _: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    _: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
) -> SearchPathsResponse {
    SearchPathsResponse::terminal(SearchPathsState::Unavailable, "platform_unavailable")
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn search_with_metadata(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
    cancelled: &dyn Fn() -> bool,
    authorize_resource: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    authorize_external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
) -> SearchPathsResponse {
    unix::search_with_metadata(
        request,
        grant,
        cancelled,
        authorize_resource,
        authorize_external,
        true,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod unix {
    use super::{
        Component, Duration, Instant, Path, PathBuf, SearchPathEntry, SearchPathKind,
        SearchPathsGrant, SearchPathsRequest, SearchPathsResponse, SearchPathsState,
        WorkspaceResourceGrant, excluded, git_policy, ignore_policy, inventory, matcher, prepared,
        valid_relative, valid_request, workspace_inventory,
    };
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Read as _;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt as _;

    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: fdopendir transferred ownership to this unique RAII wrapper.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    fn open_child(parent: &File, name: &Path) -> std::io::Result<File> {
        let name = CString::new(name.as_os_str().as_bytes())?;
        // SAFETY: parent and NUL-terminated name are live; no creation flags.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a unique owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn read_policy(
        resource: &WorkspaceResourceGrant,
        stat: &libc::stat,
        max_bytes: usize,
        tick: &mut dyn FnMut() -> Result<(), &'static str>,
    ) -> Result<Vec<u8>, &'static str> {
        // Re-traverse the grant's exact absolute target with existing no-follow machinery.
        let Ok((_, input)) =
            super::super::confined::open_target(&resource.workspace_root.join(&resource.path))
        else {
            return Err("ignore_policy_unavailable");
        };
        let Ok(metadata) = input.metadata() else {
            return Err("ignore_policy_unavailable");
        };
        #[cfg(target_os = "macos")]
        let device = u64::try_from(stat.st_dev).ok();
        #[cfg(not(target_os = "macos"))]
        let device = Some(stat.st_dev);
        if !metadata.is_file()
            || metadata.len() > 16_384
            || metadata.nlink() != 1
            || Some(metadata.dev()) != device
            || metadata.ino() != stat.st_ino
        {
            return Err("ignore_policy_unavailable");
        }
        tick()?;
        let length = usize::try_from(metadata.len()).map_err(|_| "byte_limit")?;
        if length.saturating_add(1) > max_bytes {
            return Err("byte_limit");
        }
        let mut bytes = Vec::with_capacity(length.saturating_add(1));
        let mut chunk = [0; 1024];
        let mut input = input;
        loop {
            tick()?;
            let remaining = max_bytes.saturating_sub(bytes.len()).min(chunk.len());
            if remaining == 0 {
                return Err("ignore_policy_changed");
            }
            let count = input
                .read(&mut chunk[..remaining])
                .map_err(|_| "ignore_policy_unavailable")?;
            if count == 0 {
                break;
            }
            if bytes.len().saturating_add(count) > length {
                return Err("ignore_policy_changed");
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        let Ok(after) = input.metadata() else {
            return Err("ignore_policy_unavailable");
        };
        if metadata.len() != after.len()
            || metadata.mtime() != after.mtime()
            || metadata.mtime_nsec() != after.mtime_nsec()
            || metadata.ctime() != after.ctime()
            || metadata.ctime_nsec() != after.ctime_nsec()
        {
            return Err("ignore_policy_changed");
        }
        Ok(bytes)
    }

    struct Budget<'a> {
        request: &'a SearchPathsRequest,
        cancelled: &'a dyn Fn() -> bool,
        authorize_resource: &'a dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
        started: Instant,
        bytes: usize,
        root_identity: Option<(u64, u64)>,
        bindings: Vec<(PathBuf, u64, u64)>,
        tracked_paths: Option<std::sync::Arc<std::collections::BTreeSet<String>>>,
        capture: Option<std::collections::BTreeSet<String>>,
        resources: std::collections::BTreeMap<String, Option<[u8; 32]>>,
        response: SearchPathsResponse,
    }

    impl<'a> Budget<'a> {
        fn new(
            request: &'a SearchPathsRequest,
            cancelled: &'a dyn Fn() -> bool,
            authorize_resource: &'a dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
        ) -> Self {
            Self {
                request,
                cancelled,
                authorize_resource,
                started: Instant::now(),
                bytes: 256,
                root_identity: None,
                bindings: Vec::new(),
                tracked_paths: None,
                capture: None,
                resources: std::collections::BTreeMap::new(),
                response: SearchPathsResponse {
                    entries: Vec::new(),
                    state: SearchPathsState::Complete,
                    reason: None,
                    visited_entries: 0,
                    effective_scope: request.scope,
                },
            }
        }
    }
    impl Budget<'_> {
        fn descend(
            &mut self,
            mut root: File,
            relative: &Path,
            grant: &SearchPathsGrant,
        ) -> Option<(File, Vec<ignore_policy::Rule>)> {
            let mut rules = Vec::new();
            let mut ancestor = PathBuf::new();
            for part in relative.components() {
                if !self.ignore_rules(&root, &ancestor, grant, &mut rules) {
                    return None;
                }
                if self.stop() {
                    return None;
                }
                let Component::Normal(name) = part else {
                    unreachable!();
                };
                ancestor.push(name);
                let ignored = self.ignored(&ancestor, true, &rules)?;
                if ignored {
                    self.response = SearchPathsResponse::terminal(
                        SearchPathsState::Unavailable,
                        "directory_ignored",
                    );
                    return None;
                }
                if let Ok(child) = open_child(&root, Path::new(name)) {
                    root = child;
                } else {
                    self.response = SearchPathsResponse::terminal(
                        SearchPathsState::Unavailable,
                        "directory_unavailable",
                    );
                    return None;
                }
            }
            Some((root, rules))
        }
        fn workspace(
            &mut self,
            root: &File,
            relative: &Path,
            grant: &SearchPathsGrant,
            rules: &[ignore_policy::Rule],
            authorize_external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
        ) {
            let policy = match git_policy::load(
                grant,
                self.authorize_resource,
                authorize_external,
                &mut || self.tick(),
            ) {
                Ok(policy) => policy,
                Err(reason) => {
                    self.mark(reason);
                    return;
                }
            };
            let incomplete = policy.incomplete;
            let mut inherited = policy.rules;
            inherited.extend_from_slice(rules);
            let rules = inherited.as_slice();
            let mut build_request = self.request.clone();
            build_request.max_entries = 250_000;
            build_request.recursive = false;
            build_request.timeout_ms = self.request.timeout_ms;
            let inventory = workspace_inventory::load(
                self.request,
                policy.fingerprint,
                self.tracked_paths.as_ref(),
                grant,
                self.cancelled,
                self.authorize_resource,
                &mut |progress| self.resume_workspace(progress, &build_request, grant, rules),
            );
            match inventory {
                Ok((paths, incomplete)) => {
                    if let Some(reason) = incomplete {
                        self.mark(&reason);
                    }
                    if let Ok(workspace_root) =
                        super::super::confined::open_directory(&grant.workspace_root)
                    {
                        self.tracked(&workspace_root, &paths, grant);
                    } else {
                        self.mark("root_unavailable");
                    }
                }
                Err(reason) => self.mark(&reason),
            }
            self.response.entries.sort_by(|left, right| {
                left.score
                    .cmp(&right.score)
                    .then_with(|| left.path.cmp(&right.path))
            });
            if let Some(reason) = incomplete {
                self.mark(reason);
            }
            self.bound(root, relative, grant);
            self.stop();
        }
        fn resume_workspace(
            &self,
            progress: &mut workspace_inventory::Progress,
            request: &SearchPathsRequest,
            grant: &SearchPathsGrant,
            inherited: &[ignore_policy::Rule],
        ) -> Result<(), String> {
            while let Some((relative, depth, rules)) = progress.pending.last().cloned() {
                let directory =
                    super::super::confined::open_directory(&grant.workspace_root.join(&relative))
                        .map_err(|_| "directory_changed")?;
                let before = workspace_inventory::directory_identity(&directory)?;
                let mut build = Budget::new(request, self.cancelled, self.authorize_resource);
                build.started = self.started;
                build.root_identity = self.root_identity;
                build.tracked_paths.clone_from(&self.tracked_paths);
                build.capture = Some(std::collections::BTreeSet::new());
                let mut rules = if progress.pending.len() == 1 && progress.directories.is_empty() {
                    inherited.to_vec()
                } else {
                    rules
                };
                if !build.ignore_rules(&directory, &relative, grant, &mut rules)
                    || !build.walk_entries(&directory, &relative, depth, grant, &mut rules)
                    || !build.bound(&directory, &relative, grant)
                {
                    return Err(build
                        .response
                        .reason
                        .unwrap_or_else(|| "inventory_incomplete".into()));
                }
                if workspace_inventory::directory_identity(&directory)? != before {
                    return Err("directory_changed".into());
                }
                let captured = build.capture.take().expect("capture active");
                if progress.paths.len() + captured.len() > 250_000 {
                    return Err("inventory_entry_limit".into());
                }
                progress.bytes = progress.bytes.saturating_add(build.bytes);
                if progress.bytes > 32 * 1024 * 1024 {
                    return Err("inventory_byte_limit".into());
                }
                progress.pending.pop();
                for path in &captured {
                    if let Some(child) = path.strip_suffix('/') {
                        if depth >= request.max_depth.clamp(1, 64) {
                            return Err("depth_limit".into());
                        }
                        progress.bytes = progress.bytes.saturating_add(
                            child.len()
                                + rules.len() * std::mem::size_of::<ignore_policy::Rule>()
                                + 128,
                        );
                        if progress.bytes > 32 * 1024 * 1024 {
                            return Err("inventory_byte_limit".into());
                        }
                        progress
                            .pending
                            .push((child.into(), depth + 1, rules.clone()));
                    }
                }
                // Successful enumeration may retain valid peers while skipping
                // non-UTF-8 names. Preserve that status with the cached paths.
                if progress.incomplete.is_none() {
                    progress.incomplete = build.response.reason.take();
                }
                progress.directories.insert(relative, before);
                progress.paths.extend(captured);
                progress.resources.extend(build.resources);
            }
            Ok(())
        }
        fn mark(&mut self, reason: &str) {
            if self.response.state == SearchPathsState::Complete {
                self.response.state = if reason == "cancelled" {
                    SearchPathsState::Cancelled
                } else {
                    SearchPathsState::Incomplete
                };
                self.response.reason = Some(reason.into());
            }
        }
        fn stop(&mut self) -> bool {
            if (self.cancelled)() {
                self.response.state = SearchPathsState::Cancelled;
                self.response.reason = Some("cancelled".into());
                return true;
            }
            if self.started.elapsed()
                >= Duration::from_millis(self.request.timeout_ms.clamp(1, 1000))
            {
                self.mark("time_limit");
                return true;
            }
            false
        }
        fn tick(&mut self) -> Result<(), &'static str> {
            if !self.stop() {
                return Ok(());
            }
            if self.response.state == SearchPathsState::Cancelled {
                Err("cancelled")
            } else {
                Err("time_limit")
            }
        }
        fn charge(&mut self, bytes: usize) -> bool {
            let Some(total) = self.bytes.checked_add(bytes) else {
                self.mark("byte_limit");
                return false;
            };
            let limit = if self.capture.is_some() {
                32 * 1024 * 1024
            } else {
                self.request.max_bytes.clamp(1024, 1_048_576)
            };
            if total > limit {
                self.mark("byte_limit");
                return false;
            }
            self.bytes = total;
            true
        }
        fn admit(&mut self, bytes: usize) -> Result<(), &'static str> {
            self.tick()?;
            if self.charge(bytes) {
                Ok(())
            } else {
                Err("byte_limit")
            }
        }
        fn bound(&mut self, file: &File, relative: &Path, grant: &SearchPathsGrant) -> bool {
            if self.stop() {
                return false;
            }
            let valid = (|| {
                if excluded(relative, grant, self.request.include_hidden) {
                    return None;
                }
                let root = super::super::confined::open_directory(&grant.workspace_root).ok()?;
                let root = root.metadata().ok()?;
                if self.root_identity != Some((root.dev(), root.ino())) {
                    return None;
                }
                let current =
                    super::super::confined::open_directory(&grant.workspace_root.join(relative))
                        .ok()?;
                let current = current.metadata().ok()?;
                let pinned = file.metadata().ok()?;
                (current.dev() == pinned.dev() && current.ino() == pinned.ino()).then_some(())
            })()
            .is_some();
            if !valid {
                self.mark("directory_changed");
                self.response.entries.clear();
            }
            valid
        }
        fn ignored(
            &mut self,
            path: &Path,
            directory: bool,
            rules: &[ignore_policy::Rule],
        ) -> Option<bool> {
            if self.tracked_paths.as_ref().is_some_and(|paths| {
                path.to_str().is_some_and(|text| paths.contains(text))
                    || (directory
                        && paths
                            .range(format!("{}/", path.display())..)
                            .next()
                            .is_some_and(|text| text.starts_with(&format!("{}/", path.display()))))
            }) {
                return Some(false);
            }
            let mut ancestor = path.parent();
            while let Some(parent) = ancestor.filter(|parent| !parent.as_os_str().is_empty()) {
                match ignore_policy::ignored(parent, true, rules, &mut || self.tick()) {
                    Ok(true) => return Some(true),
                    Ok(false) => (),
                    Err(reason) => {
                        self.mark(reason);
                        return None;
                    }
                }
                ancestor = parent.parent();
            }
            match ignore_policy::ignored(path, directory, rules, &mut || self.tick()) {
                Ok(ignored) => Some(ignored),
                Err(reason) => {
                    self.mark(reason);
                    None
                }
            }
        }
        fn remember(&mut self, file: &File, relative: &Path, grant: &SearchPathsGrant) -> bool {
            if !self.bound(file, relative, grant) {
                return false;
            }
            if !self.charge(relative.as_os_str().len().saturating_add(128)) {
                return false;
            }
            let Ok(metadata) = file.metadata() else {
                self.mark("directory_changed");
                return false;
            };
            self.bindings
                .push((relative.into(), metadata.dev(), metadata.ino()));
            true
        }
        fn validate_results(&mut self, grant: &SearchPathsGrant) {
            for index in 0..self.bindings.len() {
                if self.stop() {
                    self.response.entries.clear();
                    return;
                }
                let (relative, device, inode) = &self.bindings[index];
                let matches =
                    super::super::confined::open_directory(&grant.workspace_root.join(relative))
                        .and_then(|file| file.metadata())
                        .is_ok_and(|metadata| {
                            metadata.dev() == *device && metadata.ino() == *inode
                        });
                if !matches {
                    self.mark("directory_changed");
                    self.response.entries.clear();
                    return;
                }
            }
        }
        fn names(
            &mut self,
            file: &File,
            relative: &Path,
            grant: &SearchPathsGrant,
        ) -> Option<Vec<String>> {
            if !self.remember(file, relative, grant) {
                return None;
            }
            // SAFETY: dup creates an independently owned descriptor.
            let fd = unsafe { libc::dup(file.as_raw_fd()) };
            if fd < 0 {
                self.mark("entry_unavailable");
                return None;
            }
            // SAFETY: fd is an owned directory descriptor. fdopendir takes ownership on success.
            let dir = unsafe { libc::fdopendir(fd) };
            if dir.is_null() {
                // SAFETY: fdopendir failed and did not consume fd.
                unsafe {
                    libc::close(fd);
                }
                self.mark("entry_unavailable");
                return None;
            }
            let dir = Directory(dir);
            let mut names = Vec::new();
            loop {
                if (self.capture.is_none() || names.len() % 128 == 0)
                    && !self.bound(file, relative, grant)
                {
                    return None;
                }
                // Clear thread-local errno so EOF and enumeration failure remain distinguishable.
                set_errno(0);
                // SAFETY: directory is live and exclusively enumerated; entry read before next call.
                let entry = unsafe { libc::readdir(dir.0) };
                if entry.is_null() {
                    if errno() != 0 {
                        self.mark("entry_unavailable");
                        return None;
                    }
                    break;
                }
                // SAFETY: readdir returns a NUL-terminated d_name valid until next readdir.
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                if matches!(name.to_bytes(), b"." | b"..") {
                    continue;
                }
                if self.response.visited_entries >= self.request.max_entries.clamp(1, 250_000) {
                    self.mark("entry_limit");
                    return None;
                }
                self.response.visited_entries += 1;
                if !self.charge(name.to_bytes().len().saturating_add(64)) {
                    return None;
                }
                let Ok(name) = name.to_str() else {
                    self.mark("non_utf8_path");
                    continue;
                };
                names.push(name.to_owned());
            }
            if !self.bound(file, relative, grant) {
                return None;
            }
            names.sort();
            Some(names)
        }
        fn ignore_rules(
            &mut self,
            file: &File,
            relative: &Path,
            grant: &SearchPathsGrant,
            rules: &mut Vec<ignore_policy::Rule>,
        ) -> bool {
            if !self.bound(file, relative, grant) {
                return false;
            }
            let path = relative.join(".gitignore");
            if excluded(&path, grant, true) {
                self.mark("ignore_policy_denied");
                return false;
            }
            if self.stop() {
                return false;
            }
            self.resources
                .insert(path.to_string_lossy().into_owned(), None);
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: live descriptor/name, writable stat, no symlink following.
            if unsafe {
                libc::fstatat(
                    file.as_raw_fd(),
                    c".gitignore".as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                    return true;
                }
                self.mark("ignore_policy_unavailable");
                return false;
            }
            // SAFETY: successful fstatat initialized output.
            let stat = unsafe { stat.assume_init() };
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_size > 16_384 {
                self.mark("ignore_policy_unavailable");
                return false;
            }
            let Some(path_text) = path.to_str() else {
                self.mark("ignore_policy_unavailable");
                return false;
            };
            let Ok(length) = usize::try_from(stat.st_size) else {
                self.mark("ignore_policy_unavailable");
                return false;
            };
            let reserved = length.saturating_add(1);
            if !self.charge(reserved.saturating_mul(8).saturating_add(128)) {
                return false;
            }
            let Some(resource) = (self.authorize_resource)(path_text) else {
                self.mark("ignore_policy_denied");
                self.stop();
                return false;
            };
            if self.stop() {
                return false;
            }
            if resource.workspace_root != grant.workspace_root || resource.path != path_text {
                self.mark("ignore_grant_mismatch");
                return false;
            }
            if !self.bound(file, relative, grant) {
                return false;
            }
            let bytes = match read_policy(&resource, &stat, reserved, &mut || self.tick()) {
                Ok(bytes) => bytes,
                Err(reason) => {
                    self.mark(reason);
                    return false;
                }
            };
            if !self.bound(file, relative, grant) {
                return false;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                self.mark("ignore_policy_unsupported");
                return false;
            };
            let parsed = match ignore_policy::parse(
                relative,
                text,
                1024_usize.saturating_sub(rules.len()),
                &mut |cost| self.admit(cost),
            ) {
                Ok(parsed) => parsed,
                Err(reason) => {
                    self.mark(reason);
                    return false;
                }
            };
            self.resources.insert(
                path.to_string_lossy().into_owned(),
                Some(<sha2::Sha256 as sha2::Digest>::digest(&bytes).into()),
            );
            rules.extend(parsed);
            true
        }
        fn candidate(&mut self, path: &str, kind: SearchPathKind) -> bool {
            if let Some(capture) = self.capture.as_mut() {
                if capture.len() >= 250_000 {
                    self.mark("inventory_entry_limit");
                    return false;
                }
                if !self.charge(path.len() + 64) {
                    return false;
                }
                let captured = if kind == SearchPathKind::Directory {
                    format!("{path}/")
                } else {
                    path.into()
                };
                self.capture
                    .as_mut()
                    .expect("capture active")
                    .insert(captured);
                return true;
            }
            let request = self.request;
            let matched = match matcher::find(path, &request.query, &mut || self.tick()) {
                Ok(Some(matched)) => matched,
                Ok(None) => return true,
                Err(reason) => {
                    self.mark(reason);
                    return false;
                }
            };
            let entry = SearchPathEntry {
                path: path.into(),
                kind,
                score: matched.score,
                match_ranges: matched.ranges,
            };
            let limit = request.max_results.clamp(1, 256);
            if self.response.entries.len() == limit {
                let worst = self
                    .response
                    .entries
                    .iter()
                    .enumerate()
                    .max_by(|(_, left), (_, right)| {
                        left.score
                            .cmp(&right.score)
                            .then_with(|| left.path.cmp(&right.path))
                    })
                    .map(|(index, _)| index)
                    .expect("nonempty bounded results");
                self.mark("result_limit");
                let old = &self.response.entries[worst];
                if (entry.score, &entry.path) >= (old.score, &old.path) {
                    return true;
                }
                let old_bytes = old.path.len() * 6 + old.match_ranges.len() * 32 + 128;
                self.bytes = self.bytes.saturating_sub(old_bytes);
                self.response.entries.swap_remove(worst);
            }
            if !self.charge(entry.path.len() * 6 + entry.match_ranges.len() * 32 + 128) {
                return false;
            }
            self.response.entries.push(entry);
            true
        }
        fn rank_prepared<'a>(
            &mut self,
            prepared: &'a prepared::Prepared,
            grant: &SearchPathsGrant,
        ) -> Option<Vec<(usize, &'a str, SearchPathKind)>> {
            let query = matcher::Query::new(&self.request.query);
            let mut ranked = Vec::new();
            let directory = self.request.directory.as_str();
            let prefix = if directory.is_empty() {
                String::new()
            } else {
                format!("{directory}/")
            };
            let base_depth = directory.bytes().filter(|byte| *byte == b'/').count()
                + usize::from(!directory.is_empty());
            for (index, entry) in prepared.entries.iter().enumerate() {
                if index % 128 == 0 && self.stop() {
                    return None;
                }
                if (!self.request.include_hidden && entry.hidden)
                    || !entry.path.starts_with(&prefix)
                    || grant
                        .excluded_prefixes
                        .iter()
                        .any(|prefix| Path::new(&entry.path).starts_with(prefix))
                {
                    continue;
                }
                let depth = entry.depth - base_depth;
                if depth == 0
                    || depth > self.request.max_depth.clamp(1, 64)
                    || (!self.request.recursive && depth != 1)
                {
                    continue;
                }
                self.response.visited_entries += 1;
                if self.response.visited_entries > self.request.max_entries.clamp(1, 250_000) {
                    self.mark("entry_limit");
                    return None;
                }
                if let Some(score) = query.score(&entry.lower, entry.mask) {
                    ranked.push((score, entry.path.as_str(), entry.kind));
                }
            }
            Some(ranked)
        }
        fn tracked(
            &mut self,
            root: &File,
            paths: &std::sync::Arc<std::collections::BTreeSet<String>>,
            grant: &SearchPathsGrant,
        ) {
            let prepared = match prepared::load(
                paths,
                self.request.scope == super::SearchPathsScope::Workspace,
                &mut || self.tick(),
            ) {
                Ok(prepared) => prepared,
                Err(reason) => {
                    self.mark(reason);
                    return;
                }
            };
            let Some(mut ranked) = self.rank_prepared(&prepared, grant) else {
                return;
            };
            ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)));
            if self.stop() {
                return;
            }
            for (_, path, kind) in ranked {
                if self.response.entries.len() >= self.request.max_results.clamp(1, 256) {
                    self.mark("result_limit");
                    break;
                }
                if !self.bound(root, Path::new(""), grant) {
                    return;
                }
                let path_ref = Path::new(path);
                let Some(parent) = path_ref.parent() else {
                    continue;
                };
                let Ok(directory) =
                    super::super::confined::open_directory(&grant.workspace_root.join(parent))
                else {
                    continue;
                };
                if !self.remember(&directory, parent, grant) {
                    return;
                }
                let Some(name) = path_ref.file_name() else {
                    continue;
                };
                let Ok(name) = CString::new(name.as_bytes()) else {
                    continue;
                };
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: live descriptor/basename/output; never follow symlinks.
                if unsafe {
                    libc::fstatat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        stat.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    continue;
                }
                // SAFETY: successful fstatat initialized output.
                let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
                if (kind == SearchPathKind::File && mode != libc::S_IFREG)
                    || (kind == SearchPathKind::Directory && mode != libc::S_IFDIR)
                {
                    continue;
                }
                if !self.candidate(path, kind) {
                    return;
                }
            }
            self.validate_results(grant);
        }
        fn walk(
            &mut self,
            file: &File,
            relative: &Path,
            depth: usize,
            grant: &SearchPathsGrant,
            rules: &mut Vec<ignore_policy::Rule>,
        ) -> bool {
            let inherited = rules.len();
            if !self.ignore_rules(file, relative, grant, rules) {
                return false;
            }
            let result = self.walk_entries(file, relative, depth, grant, rules);
            let bound = self.bound(file, relative, grant);
            rules.truncate(inherited);
            result && bound
        }
        fn walk_entries(
            &mut self,
            file: &File,
            relative: &Path,
            depth: usize,
            grant: &SearchPathsGrant,
            rules: &mut Vec<ignore_policy::Rule>,
        ) -> bool {
            let Some(names) = self.names(file, relative, grant) else {
                return false;
            };
            for (index, name) in names.into_iter().enumerate() {
                if (self.capture.is_none() || index % 128 == 0)
                    && !self.bound(file, relative, grant)
                {
                    return false;
                }
                let path = relative.join(&name);
                if excluded(&path, grant, self.request.include_hidden) {
                    continue;
                }
                let Ok(cname) = CString::new(name.as_bytes()) else {
                    self.mark("non_utf8_path");
                    continue;
                };
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: output is writable; file/name remain live. Never follows symlinks.
                if unsafe {
                    libc::fstatat(
                        file.as_raw_fd(),
                        cname.as_ptr(),
                        stat.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    self.mark("entry_unavailable");
                    continue;
                }
                // SAFETY: successful fstatat initialized the structure.
                let mode = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
                let kind = match mode {
                    libc::S_IFREG => SearchPathKind::File,
                    libc::S_IFDIR => SearchPathKind::Directory,
                    _ => continue,
                };
                match self.ignored(&path, kind == SearchPathKind::Directory, rules) {
                    Some(true) => continue,
                    Some(false) => (),
                    None => return false,
                }
                if self.capture.is_none() && !self.bound(file, relative, grant) {
                    return false;
                }
                let path_text = path.to_str().expect("validated UTF-8 path");
                if !self.candidate(path_text, kind) {
                    return false;
                }
                if kind == SearchPathKind::Directory && self.request.recursive {
                    if depth >= self.request.max_depth.clamp(1, 64) {
                        self.mark("depth_limit");
                        continue;
                    }
                    match open_child(file, Path::new(&name)) {
                        Ok(child) => {
                            if !self.walk(&child, &path, depth + 1, grant, rules) {
                                return false;
                            }
                        }
                        Err(_) => self.mark("entry_unavailable"),
                    }
                }
            }
            true
        }
    }

    // libc exposes the platform's thread-local errno through different accessors.
    #[cfg(target_os = "macos")]
    fn errno_pointer() -> *mut libc::c_int {
        unsafe { libc::__error() }
    }
    #[cfg(not(target_os = "macos"))]
    fn errno_pointer() -> *mut libc::c_int {
        unsafe { libc::__errno_location() }
    }
    fn set_errno(value: libc::c_int) {
        unsafe {
            *errno_pointer() = value;
        }
    }
    fn errno() -> libc::c_int {
        unsafe { *errno_pointer() }
    }

    fn valid_grant(grant: &SearchPathsGrant) -> bool {
        grant.workspace_root.is_absolute()
            && grant
                .workspace_root
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
            && grant.workspace_root.as_os_str().len() <= 4096
            && grant.workspace_root.components().count() <= 64
            && grant.excluded_prefixes.len() <= 4096
            && grant
                .excluded_prefixes
                .iter()
                .all(|prefix| valid_relative(prefix))
            && grant
                .excluded_prefixes
                .iter()
                .map(|prefix| prefix.as_os_str().len())
                .sum::<usize>()
                <= 262_144
    }

    pub(super) fn search_with_metadata(
        request: &SearchPathsRequest,
        grant: &SearchPathsGrant,
        cancelled: &dyn Fn() -> bool,
        authorize_resource: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
        authorize_external: &dyn Fn(&Path) -> Option<WorkspaceResourceGrant>,
        use_inventory: bool,
    ) -> SearchPathsResponse {
        let mut budget = Budget::new(request, cancelled, authorize_resource);
        if budget.stop() {
            return budget.response;
        }
        if !valid_request(request) || !valid_grant(grant) {
            return SearchPathsResponse::terminal(SearchPathsState::Unavailable, "invalid_grant");
        }
        let relative = Path::new(&request.directory);
        if excluded(relative, grant, request.include_hidden) {
            return SearchPathsResponse::terminal(
                SearchPathsState::Unavailable,
                "directory_denied",
            );
        }
        // Reuse canonical no-follow descriptor traversal; never canonicalize untrusted
        // paths via symlink-following filesystem operations. No regular file is opened.
        let Ok(root) = super::super::confined::open_directory(&grant.workspace_root) else {
            return SearchPathsResponse::terminal(
                SearchPathsState::Unavailable,
                "root_unavailable",
            );
        };
        let Ok(metadata) = root.metadata() else {
            return SearchPathsResponse::terminal(
                SearchPathsState::Unavailable,
                "root_unavailable",
            );
        };
        if !metadata.is_dir() {
            return SearchPathsResponse::terminal(
                SearchPathsState::Unavailable,
                "root_unavailable",
            );
        }
        budget.root_identity = Some((metadata.dev(), metadata.ino()));
        let tracked = if use_inventory {
            match inventory::load(
                request,
                grant,
                authorize_resource,
                authorize_external,
                &mut || budget.tick(),
            ) {
                Ok(paths) => paths,
                Err(reason) => {
                    budget.mark(reason);
                    return budget.response;
                }
            }
        } else {
            None
        };
        if request.scope == super::SearchPathsScope::Tracked {
            let Some(paths) = tracked else {
                let mut response =
                    SearchPathsResponse::terminal(SearchPathsState::Unavailable, "non_repository");
                response.effective_scope = request.scope;
                return response;
            };
            budget.tracked(&root, &paths, grant);
            budget.bound(&root, Path::new(""), grant);
            budget.stop();
            budget.response.entries.sort_by(|left, right| {
                left.score
                    .cmp(&right.score)
                    .then_with(|| left.path.cmp(&right.path))
            });
            return budget.response;
        }
        budget.tracked_paths = tracked;
        let Some((root, mut rules)) = budget.descend(root, relative, grant) else {
            return budget.response;
        };
        if use_inventory && request.scope == super::SearchPathsScope::Workspace {
            budget.workspace(&root, relative, grant, &rules, authorize_external);
            return budget.response;
        }
        budget.walk(&root, relative, 1, grant, &mut rules);
        budget.bound(&root, relative, grant);
        budget.validate_results(grant);
        budget.stop();
        budget.response.entries.sort_by(|left, right| {
            left.score
                .cmp(&right.score)
                .then_with(|| left.path.cmp(&right.path))
        });
        budget.response
    }
}

#[cfg(all(
    test,
    any(target_os = "macos", target_os = "linux", target_os = "android")
))]
mod tests;

#[cfg(test)]
fn search(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
    cancelled: &dyn Fn() -> bool,
    authorize_resource: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
) -> SearchPathsResponse {
    unix::search_with_metadata(
        request,
        grant,
        cancelled,
        authorize_resource,
        &|_| None,
        false,
    )
}

#[cfg(test)]
fn inventory_search(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
    cancelled: &dyn Fn() -> bool,
    authorize_resource: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
) -> SearchPathsResponse {
    search_with_metadata(request, grant, cancelled, authorize_resource, &|_| None)
}
