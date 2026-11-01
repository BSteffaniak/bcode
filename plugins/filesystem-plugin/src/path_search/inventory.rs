//! Bounded Git-index path inventory; content reads require fresh resource grants.
//! Single-slot cache coalesces refreshes and never caches authorization.
use super::{SearchPathsGrant, SearchPathsRequest, WorkspaceResourceGrant, valid_relative};
use bcode_confined_fs::{EntryKind, FileIdentity, Metadata, Timestamp, file_metadata};
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATHS: usize = 250_000;
type Identity = (FileIdentity, u64, Timestamp, Timestamp);
const fn identity(metadata: &Metadata) -> Identity {
    (
        metadata.identity(),
        metadata.len(),
        metadata.modified(),
        metadata.changed(),
    )
}
struct Cached {
    root: PathBuf,
    root_identity: FileIdentity,
    index: Identity,
    index_path: PathBuf,
    pointer: Option<(PathBuf, Identity)>,
    built: Instant,
    paths: Arc<BTreeSet<String>>,
}
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// None means no `.git` entry. Gitfiles resolve only after an exact pointer grant;
/// their external index requires a fresh host-produced exact-resource grant.
pub(super) fn load(
    request: &SearchPathsRequest,
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &dyn Fn(&std::path::Path) -> Option<WorkspaceResourceGrant>,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Option<Arc<BTreeSet<String>>>, &'static str> {
    let refresh_started = Instant::now();
    tick()?;
    let root = super::super::confined::open_directory(&grant.workspace_root)
        .map_err(|_| "root_unavailable")?;
    let root_meta = root.metadata().map_err(|_| "root_unavailable")?;
    let repository_root = discover_repository(&grant.workspace_root, tick)?;
    let Some(repository_root) = repository_root else {
        return Ok(None);
    };
    let Some((resource, pointer)) =
        resolve_index(grant, authorize, external, tick, &repository_root)?
    else {
        return Ok(Some(Arc::new(BTreeSet::new())));
    };
    let index_path = super::under(&resource.workspace_root, &resource.path);
    tick()?;
    let (_, mut index) = match super::super::confined::open_target(&super::under(
        &resource.workspace_root,
        &resource.path,
    )) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some(Arc::new(BTreeSet::new())));
        }
        Err(_) => return Err("git_index_unavailable"),
    };
    let metadata = file_metadata(&index).map_err(|_| "git_index_unavailable")?;
    if metadata.kind() != EntryKind::File || metadata.len() > MAX_INDEX_BYTES as u64 {
        return Err("git_index_limit");
    }
    let stamp = identity(&metadata);
    let root_stamp = root_meta.identity();
    let mut cache = loop {
        tick()?;
        match CACHE.try_lock() {
            Ok(cache) => break cache,
            Err(TryLockError::Poisoned(_)) => return Err("inventory_unavailable"),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    };
    if let Some(cached) = cache.as_ref().filter(|cached| {
        (!request.refresh || cached.built >= refresh_started)
            && cached.root == grant.workspace_root
            && cached.root_identity == root_stamp
            && cached.index == stamp
            && cached.index_path == index_path
            && cached.pointer == pointer
            && cached.built.elapsed() < Duration::from_secs(5)
    }) {
        return Ok(Some(Arc::clone(&cached.paths)));
    }
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        tick()?;
        let count = index
            .read(&mut chunk)
            .map_err(|_| "git_index_unavailable")?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > MAX_INDEX_BYTES {
            return Err("git_index_limit");
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let paths = confine_paths(&bytes, grant, &repository_root, tick)?;
    tick()?;
    let (_, current) = super::super::confined::open_target(&super::under(
        &resource.workspace_root,
        &resource.path,
    ))
    .map_err(|_| "git_index_changed")?;
    let current_root = super::super::confined::open_directory(&grant.workspace_root)
        .map_err(|_| "root_changed")?;
    let current_root = current_root.metadata().map_err(|_| "root_changed")?;
    if identity(&file_metadata(&current).map_err(|_| "git_index_changed")?) != stamp
        || current_root.identity() != root_stamp
    {
        return Err("git_index_changed");
    }
    if let Some((path, stamp)) = &pointer {
        let (_, current) =
            super::super::confined::open_target(path).map_err(|_| "git_pointer_changed")?;
        if identity(&file_metadata(&current).map_err(|_| "git_pointer_changed")?) != *stamp {
            return Err("git_pointer_changed");
        }
    }
    *cache = Some(Cached {
        root: grant.workspace_root.clone(),
        root_identity: root_stamp,
        index: stamp,
        index_path,
        pointer,
        built: Instant::now(),
        paths: Arc::clone(&paths),
    });
    drop(cache);
    Ok(Some(paths))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, &'static str> {
    let value: [u8; 4] = bytes
        .get(at..at + 4)
        .ok_or("git_index_invalid")?
        .try_into()
        .map_err(|_| "git_index_invalid")?;
    Ok(u32::from_be_bytes(value))
}
fn parse(
    bytes: &[u8],
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<BTreeSet<String>, &'static str> {
    let hash_bytes = verify_checksum(bytes, tick)?;
    let (version, count) = index_header(bytes)?;
    let tail = bytes.len() - hash_bytes;
    let mut cursor = 12;
    let mut previous = Vec::new();
    let mut paths = BTreeSet::new();
    let mut stored = 0_usize;
    for _ in 0..count {
        tick()?;
        let start = cursor;
        let fixed_bytes = 42 + hash_bytes;
        let entry = bytes
            .get(cursor..cursor + fixed_bytes)
            .filter(|_| cursor + fixed_bytes <= tail)
            .ok_or("git_index_invalid")?;
        let mode = u32_at(entry, 24)? & 0o170_000;
        let flags = u16::from_be_bytes([entry[fixed_bytes - 2], entry[fixed_bytes - 1]]);
        cursor += fixed_bytes;
        if flags & 0x4000 != 0 {
            if version == 2 {
                return Err("git_index_invalid");
            }
            cursor = cursor
                .checked_add(2)
                .filter(|at| *at <= tail)
                .ok_or("git_index_invalid")?;
        }
        let mut name = Vec::new();
        if version == 4 {
            let mut remove = 0_usize;
            loop {
                tick()?;
                let byte = *bytes
                    .get(cursor)
                    .filter(|_| cursor < tail)
                    .ok_or("git_index_invalid")?;
                cursor += 1;
                remove = remove
                    .checked_mul(128)
                    .and_then(|n| n.checked_add(usize::from(byte & 0x7f)))
                    .ok_or("git_index_invalid")?;
                if byte & 0x80 == 0 {
                    break;
                }
                remove = remove.checked_add(1).ok_or("git_index_invalid")?;
            }
            let keep = previous
                .len()
                .checked_sub(remove)
                .ok_or("git_index_invalid")?;
            name.extend_from_slice(&previous[..keep]);
        }
        loop {
            tick()?;
            let byte = *bytes
                .get(cursor)
                .filter(|_| cursor < tail)
                .ok_or("git_index_invalid")?;
            cursor += 1;
            if byte == 0 {
                break;
            }
            if name.len() >= 4096 {
                return Err("git_path_limit");
            }
            name.push(byte);
        }
        if version != 4 {
            let length = cursor - start;
            cursor = start + length.div_ceil(8) * 8;
            if cursor > tail {
                return Err("git_index_invalid");
            }
        }
        previous.clone_from(&name);
        let name = String::from_utf8(name).map_err(|_| "non_utf8_path")?;
        if name.is_empty() {
            return Err("git_index_extension_unsupported");
        }
        if !valid_relative(std::path::Path::new(&name)) || name.is_empty() {
            return Err("git_index_invalid");
        }
        if mode == 0o100_000 {
            stored = stored.saturating_add(name.len() + 64);
            if stored > MAX_PATH_BYTES {
                return Err("inventory_byte_limit");
            }
            paths.insert(name);
        } else if mode == 0o040_000 {
            return Err("git_sparse_index_unsupported");
        }
    }
    validate_extensions(bytes, cursor, tail, tick)?;
    Ok(paths)
}

/// Lexically resolve `target` (absolute, or relative to the canonical `root`).
///
/// The base and target are normalized as separate component streams: joining a relative
/// target onto a Windows verbatim (`\\?\`) root would otherwise keep `/` and `..`
/// literally inside one component. Only the base may carry a drive or UNC prefix.
pub(super) fn normalize_absolute(
    root: &std::path::Path,
    target: &str,
) -> Result<PathBuf, &'static str> {
    use std::path::Component;
    let target = std::path::Path::new(target);
    let (base, relative) = if target.is_absolute() {
        (target, std::path::Path::new(""))
    } else {
        (root, target)
    };
    let mut components = base.components().peekable();
    let mut normalized = match components.peek() {
        Some(Component::Prefix(prefix)) => {
            let mut normalized = PathBuf::from(prefix.as_os_str());
            components.next();
            normalized.push(Component::RootDir);
            normalized
        }
        _ => PathBuf::from("/"),
    };
    for part in components.chain(relative.components()) {
        match part {
            Component::Normal(name) => normalized.push(name),
            Component::RootDir | Component::CurDir => (),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err("git_pointer_invalid");
                }
            }
            Component::Prefix(_) => return Err("git_pointer_invalid"),
        }
    }
    if !normalized.is_absolute() || normalized.components().count() > 64 {
        return Err("git_pointer_invalid");
    }
    Ok(normalized)
}

/// Metadata-only no-follow ancestor discovery; contents always need exact grants.
pub(super) fn discover_repository(
    root: &std::path::Path,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Option<PathBuf>, &'static str> {
    for ancestor in root.ancestors().take(64) {
        tick()?;
        let directory = super::super::confined::open_directory(ancestor)
            .map_err(|_| "git_ancestor_unavailable")?;
        // Never follows links: a linked `.git` is reported, not resolved.
        match directory.metadata_at(std::ffi::OsStr::new(".git")) {
            Ok(stat) => {
                if !matches!(stat.kind(), EntryKind::File | EntryKind::Directory) {
                    return Err("git_metadata_unavailable");
                }
                return Ok(Some(ancestor.to_owned()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("git_ancestor_unavailable"),
        }
    }
    Ok(None)
}

type PointerIdentity = Option<(PathBuf, Identity)>;
type ResolvedIndex = (WorkspaceResourceGrant, PointerIdentity);
fn resolve_index(
    grant: &SearchPathsGrant,
    authorize: &dyn Fn(&str) -> Option<WorkspaceResourceGrant>,
    external: &dyn Fn(&std::path::Path) -> Option<WorkspaceResourceGrant>,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
    repository_root: &std::path::Path,
) -> Result<Option<ResolvedIndex>, &'static str> {
    let git = repository_root.join(".git");
    let nested = repository_root != grant.workspace_root;
    let resolved = match super::super::confined::open_directory(&git) {
        Ok(git_directory) => {
            match git_directory.metadata_at(std::ffi::OsStr::new("index")) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("git_index_unavailable"),
            }
            let resource = if nested {
                external(&git.join("index"))
                    .filter(|resource| resource.workspace_root == git && resource.path == "index")
            } else {
                authorize(".git/index")
            }
            .filter(|resource| {
                (nested && resource.workspace_root == git && resource.path == "index")
                    || (!nested
                        && resource.workspace_root == grant.workspace_root
                        && resource.path == ".git/index")
            })
            .ok_or("git_index_denied")?;
            (resource, None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            let resource = if nested {
                external(&git)
            } else {
                authorize(".git")
            }
            .filter(|resource| super::under(&resource.workspace_root, &resource.path) == git)
            .ok_or("git_pointer_denied")?;
            let (_, mut pointer) = super::super::confined::open_target(&super::under(
                &resource.workspace_root,
                &resource.path,
            ))
            .map_err(|_| "git_pointer_unavailable")?;
            let before = file_metadata(&pointer).map_err(|_| "git_pointer_unavailable")?;
            if before.kind() != EntryKind::File || before.len() > 4096 {
                return Err("git_pointer_invalid");
            }
            let mut bytes = Vec::new();
            pointer
                .by_ref()
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(|_| "git_pointer_unavailable")?;
            tick()?;
            if bytes.len() > 4096
                || identity(&file_metadata(&pointer).map_err(|_| "git_pointer_changed")?)
                    != identity(&before)
            {
                return Err("git_pointer_changed");
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| "git_pointer_invalid")?;
            let target = text
                .strip_prefix("gitdir: ")
                .ok_or("git_pointer_invalid")?
                .trim_end_matches(['\r', '\n']);
            if target.is_empty() || target.contains(['\r', '\n', '\0']) {
                return Err("git_pointer_invalid");
            }
            let target = normalize_absolute(repository_root, target)?;
            let index_path = target.join("index");
            let resource = external(&index_path)
                .filter(|resource| {
                    super::under(&resource.workspace_root, &resource.path) == index_path
                        && resource.path == "index"
                        && resource.workspace_root == target
                })
                .ok_or("git_index_denied")?;
            (resource, Some((git.clone(), identity(&before))))
        }
    };
    Ok(Some(resolved))
}

fn verify_checksum(
    bytes: &[u8],
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<usize, &'static str> {
    if bytes.get(..4) != Some(b"DIRC") || bytes.len() < 32 {
        return Err("git_index_invalid");
    }
    let mut sha1 = sha1_smol::Sha1::new();
    for chunk in bytes[..bytes.len() - 20].chunks(8192) {
        tick()?;
        sha1.update(chunk);
    }
    tick()?;
    if sha1.digest().bytes().as_slice() == &bytes[bytes.len() - 20..] {
        return Ok(20);
    }
    if bytes.len() >= 44 {
        use sha2::Digest as _;
        let mut sha256 = sha2::Sha256::new();
        for chunk in bytes[..bytes.len() - 32].chunks(8192) {
            tick()?;
            sha256.update(chunk);
        }
        tick()?;
        let digest: [u8; 32] = sha256.finalize().into();
        if digest.as_slice() == &bytes[bytes.len() - 32..] {
            return Ok(32);
        }
    }
    Err("git_index_checksum_or_object_format_unsupported")
}

fn validate_extensions(
    bytes: &[u8],
    mut cursor: usize,
    tail: usize,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    // Required lowercase extensions (split index etc.) cannot be silently ignored.
    while cursor < tail {
        tick()?;
        let header = bytes
            .get(cursor..cursor + 8)
            .filter(|_| cursor + 8 <= tail)
            .ok_or("git_index_invalid")?;
        if !header[0].is_ascii_uppercase() {
            return Err("git_index_extension_unsupported");
        }
        let length = usize::try_from(u32_at(header, 4)?).map_err(|_| "git_index_invalid")?;
        cursor = cursor
            .checked_add(8)
            .and_then(|at| at.checked_add(length))
            .filter(|at| *at <= tail)
            .ok_or("git_index_invalid")?;
    }
    Ok(())
}

fn confine_paths(
    bytes: &[u8],
    grant: &SearchPathsGrant,
    repository_root: &std::path::Path,
    tick: &mut dyn FnMut() -> Result<(), &'static str>,
) -> Result<Arc<BTreeSet<String>>, &'static str> {
    tick()?;
    let mut work = 0_usize;
    let parsed = parse(bytes, &mut || {
        work += 1;
        if work.is_multiple_of(128) {
            tick()?;
        }
        Ok(())
    })?;
    tick()?;
    let prefix = grant
        .workspace_root
        .strip_prefix(repository_root)
        .map_err(|_| "git_root_mismatch")?;
    let mut confined = BTreeSet::new();
    for path in parsed {
        tick()?;
        if let Ok(relative) = std::path::Path::new(&path).strip_prefix(prefix)
            && !relative.as_os_str().is_empty()
        {
            confined.insert(relative.to_str().ok_or("non_utf8_path")?.to_owned());
        }
    }
    Ok(Arc::new(confined))
}

fn index_header(bytes: &[u8]) -> Result<(u32, usize), &'static str> {
    let version = u32_at(bytes, 4)?;
    if !(2..=4).contains(&version) {
        return Err("git_index_unsupported");
    }
    let count = usize::try_from(u32_at(bytes, 8)?).map_err(|_| "git_index_limit")?;
    if count > MAX_PATHS {
        return Err("git_index_limit");
    }
    Ok((version, count))
}
