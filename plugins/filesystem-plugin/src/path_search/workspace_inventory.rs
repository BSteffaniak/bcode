//! Reusable authorized workspace traversal snapshots, independent of query/output bounds.
use super::{SearchPathsGrant, SearchPathsRequest, WorkspaceResourceGrant};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};
pub(super) type ResourceIdentity = (u64, u64, u64, i64, i64);
type CapturedInventory = (BTreeSet<String>, PolicyDigests);
pub(super) type PolicyDigests = BTreeMap<String, Option<[u8; 32]>>;
pub(super) struct Progress {
    pub pending: Vec<(PathBuf, usize, Vec<super::ignore_policy::Rule>)>,
    pub paths: BTreeSet<String>,
    pub resources: PolicyDigests,
    pub directories: BTreeMap<PathBuf, ResourceIdentity>,
    pub bytes: usize,
}
impl Progress {
    pub fn new(directory: &str) -> Self {
        Self {
            pending: vec![(directory.into(), 1, Vec::new())],
            paths: BTreeSet::new(),
            resources: BTreeMap::new(),
            directories: BTreeMap::new(),
            bytes: 0,
        }
    }
}
struct Pending {
    key: SnapshotKey,
    started: Instant,
    tracked: Option<Arc<BTreeSet<String>>>,
    progress: Progress,
}
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
#[derive(Clone, PartialEq, Eq)]
struct SnapshotKey {
    root: PathBuf,
    root_identity: (u64, u64),
    exclusions: Vec<PathBuf>,
    directory: String,
    hidden: bool,
    depth: usize,
    policy: [u8; 32],
}
struct Snapshot {
    key: SnapshotKey,
    built: Instant,
    paths: Arc<BTreeSet<String>>,
    tracked: Option<Arc<BTreeSet<String>>>,
    resources: BTreeMap<String, Option<ResourceIdentity>>,
}
static CACHE: Mutex<Option<Snapshot>> = Mutex::new(None);

pub(super) fn load(
    request: &SearchPathsRequest,
    policy: [u8; 32],
    tracked: Option<&Arc<BTreeSet<String>>>,
    grant: &SearchPathsGrant,
    cancelled: &dyn Fn() -> bool,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    build: &mut dyn FnMut(&mut Progress) -> Result<(), String>,
) -> Result<Arc<BTreeSet<String>>, String> {
    let started = Instant::now();
    let tick = || {
        if cancelled() {
            Err("cancelled")
        } else if started.elapsed() >= Duration::from_millis(request.timeout_ms.clamp(1, 1000)) {
            Err("time_limit")
        } else {
            Ok(())
        }
    };
    tick()?;
    let root = super::super::confined::open_directory(&grant.workspace_root)
        .map_err(|_| "root_unavailable")?;
    let metadata = root.metadata().map_err(|_| "root_unavailable")?;
    let key = SnapshotKey {
        root: grant.workspace_root.clone(),
        root_identity: (metadata.dev(), metadata.ino()),
        exclusions: grant.excluded_prefixes.clone(),
        directory: request.directory.clone(),
        hidden: request.include_hidden,
        depth: request.max_depth,
        policy,
    };
    let mut cache = loop {
        tick()?;
        match CACHE.try_lock() {
            Ok(cache) => break cache,
            Err(TryLockError::Poisoned(_)) => return Err("inventory_unavailable".into()),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    if let Some(snapshot) = cache.as_ref().filter(|snapshot| {
        snapshot.key == key
            && snapshot.tracked.as_ref().map(Arc::as_ptr) == tracked.map(Arc::as_ptr)
            && (!request.refresh || snapshot.built >= started)
            && snapshot.built.elapsed() < Duration::from_secs(5)
    }) {
        let mut unchanged = true;
        for (path, stamp) in &snapshot.resources {
            tick()?;
            if resource_stamp(grant, path, authorize)? != *stamp {
                unchanged = false;
            }
        }
        if unchanged {
            return Ok(Arc::clone(&snapshot.paths));
        }
    }
    let (paths, resources) =
        resume_capture(request, tracked, &key, grant, authorize, build, &tick)?;
    tick()?;
    let stamps = publication_stamps(resources, grant, authorize, &tick)?;
    let paths = Arc::new(paths);
    *cache = Some(Snapshot {
        key,
        built: Instant::now(),
        tracked: tracked.cloned(),
        paths: Arc::clone(&paths),
        resources: stamps,
    });
    drop(cache);
    Ok(paths)
}
fn resource_stamp(
    grant: &SearchPathsGrant,
    path: &str,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
) -> Result<Option<ResourceIdentity>, &'static str> {
    let target = grant.workspace_root.join(path);
    let parent = target.parent().ok_or("inventory_resource_changed")?;
    let directory =
        super::super::confined::open_directory(parent).map_err(|_| "inventory_resource_changed")?;
    let name = std::ffi::CString::new(
        path.rsplit('/')
            .next()
            .ok_or("inventory_resource_changed")?,
    )
    .map_err(|_| "inventory_resource_changed")?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: owned descriptor, live basename, writable stat, no symlink following.
    if unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(&directory),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err("inventory_resource_changed");
    }
    let resource = authorize(path)
        .filter(|resource| resource.workspace_root == grant.workspace_root && resource.path == path)
        .ok_or("inventory_resource_denied")?;
    let (_, file) =
        super::super::confined::open_target(&PathBuf::from(&resource.workspace_root).join(path))
            .map_err(|_| "inventory_resource_changed")?;
    let metadata = file.metadata().map_err(|_| "inventory_resource_changed")?;
    Ok(Some((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )))
}

fn validate_progress(
    progress: &Progress,
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    tick: &dyn Fn() -> Result<(), &'static str>,
) -> Result<(), String> {
    for (path, expected) in &progress.directories {
        tick()?;
        let directory = super::super::confined::open_directory(&grant.workspace_root.join(path))
            .map_err(|_| "directory_changed")?;
        if directory_identity(&directory)? != *expected {
            return Err("directory_changed".into());
        }
    }
    for (path, digest) in &progress.resources {
        tick()?;
        let stamp = resource_stamp(grant, path, authorize)?;
        if digest.is_some() != stamp.is_some() {
            return Err("inventory_resource_changed".into());
        }
        if let Some(expected) = digest {
            let (_, file) = super::super::confined::open_target(&grant.workspace_root.join(path))
                .map_err(|_| "inventory_resource_changed")?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, 16_385), &mut bytes)
                .map_err(|_| "inventory_resource_changed")?;
            let actual: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&bytes).into();
            if bytes.len() > 16_384 || actual != *expected {
                return Err("inventory_resource_changed".into());
            }
        }
    }
    Ok(())
}
pub(super) fn directory_identity(file: &std::fs::File) -> Result<ResourceIdentity, &'static str> {
    let metadata = file.metadata().map_err(|_| "directory_changed")?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

fn resume_capture(
    request: &SearchPathsRequest,
    tracked: Option<&Arc<BTreeSet<String>>>,
    key: &SnapshotKey,
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    build: &mut dyn FnMut(&mut Progress) -> Result<(), String>,
    tick: &dyn Fn() -> Result<(), &'static str>,
) -> Result<CapturedInventory, String> {
    let started = Instant::now();
    let mut pending = PENDING.lock().map_err(|_| "inventory_unavailable")?;
    if pending.as_ref().is_none_or(|pending| {
        pending.key != *key
            || request.refresh
            || pending.started.elapsed() > Duration::from_secs(5)
            || pending.tracked.as_ref().map(Arc::as_ptr) != tracked.map(Arc::as_ptr)
    }) {
        *pending = Some(Pending {
            key: key.clone(),
            started,
            tracked: tracked.cloned(),
            progress: Progress::new(&request.directory),
        });
    }
    let progress = &mut pending.as_mut().expect("pending initialized").progress;
    let outcome =
        validate_progress(progress, grant, authorize, tick).and_then(|()| build(progress));
    if let Err(reason) = outcome {
        if reason != "time_limit" {
            *pending = None;
        }
        return Err(reason);
    }
    let completed = pending.take().expect("pending initialized").progress;
    drop(pending);
    let (paths, resources) = (completed.paths, completed.resources);
    Ok((paths, resources))
}

fn publication_stamps(
    resources: PolicyDigests,
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    tick: &dyn Fn() -> Result<(), &'static str>,
) -> Result<BTreeMap<String, Option<ResourceIdentity>>, String> {
    let mut stamps = BTreeMap::new();
    for (path, expected) in resources {
        tick()?;
        let stamp = resource_stamp(grant, &path, authorize)?;
        if expected.is_some() != stamp.is_some() {
            return Err("inventory_resource_changed".into());
        }
        if let Some(expected) = expected {
            let (_, file) = super::super::confined::open_target(&grant.workspace_root.join(&path))
                .map_err(|_| "inventory_resource_changed")?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, 16_385), &mut bytes)
                .map_err(|_| "inventory_resource_changed")?;
            tick()?;
            let actual: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&bytes).into();
            if bytes.len() > 16_384
                || actual != expected
                || resource_stamp(grant, &path, authorize)? != stamp
            {
                return Err("inventory_resource_changed".into());
            }
        }
        stamps.insert(path, stamp);
    }
    Ok(stamps)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuation_retains_completed_paths_but_discards_on_permission_failure() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let grant = SearchPathsGrant {
            workspace_root: root,
            excluded_prefixes: Vec::new(),
        };
        let request = SearchPathsRequest::default();
        let metadata = super::super::super::confined::open_directory(&grant.workspace_root)
            .unwrap()
            .metadata()
            .unwrap();
        let key = SnapshotKey {
            root: grant.workspace_root.clone(),
            root_identity: (metadata.dev(), metadata.ino()),
            exclusions: Vec::new(),
            directory: String::new(),
            hidden: false,
            depth: 32,
            policy: [0; 32],
        };
        let first = resume_capture(
            &request,
            None,
            &key,
            &grant,
            &|_| None,
            &mut |progress| {
                progress.paths.insert("completed".into());
                Err("time_limit".into())
            },
            &|| Ok(()),
        );
        assert_eq!(first.unwrap_err(), "time_limit");
        let extended_request = SearchPathsRequest {
            query: "extended-query".into(),
            ..request.clone()
        };
        let second = resume_capture(
            &extended_request,
            None,
            &key,
            &grant,
            &|_| None,
            &mut |progress| {
                assert!(progress.paths.contains("completed"));
                Err("inventory_resource_denied".into())
            },
            &|| Ok(()),
        );
        assert_eq!(second.unwrap_err(), "inventory_resource_denied");
        let third = resume_capture(
            &request,
            None,
            &key,
            &grant,
            &|_| None,
            &mut |progress| {
                assert!(progress.paths.is_empty());
                Ok(())
            },
            &|| Ok(()),
        );
        assert!(third.unwrap().0.is_empty());
    }
}
