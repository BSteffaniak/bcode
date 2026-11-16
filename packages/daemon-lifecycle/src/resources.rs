//! Content-addressed snapshots of artifact resource trees for daemon images.
//!
//! A daemon image pins the executable bytes; this module pins the resource
//! trees that belong to the same artifact (see
//! [`bcode_plugin_sdk::artifact_resources`]). The snapshot lives inside the
//! image directory as `resources-<sha256>`, so image retention removes it with
//! the executable and an unchanged tree is published once.

use crate::DaemonLifecycleError;
use bcode_plugin_sdk::artifact_resources::{ARTIFACT_RESOURCE_TREES, artifact_resource_root};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const COMPLETE_MARKER: &str = ".bcode-resources-complete";
const MAX_DEPTH: usize = 32;
const MAX_FILES: usize = 50_000;

/// One regular file in a resource snapshot, keyed by its `/`-separated relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceFile {
    source: PathBuf,
    bytes: u64,
    sha256: String,
    executable: bool,
}

type ResourceManifest = BTreeMap<String, ResourceFile>;

/// Publishes the current process's artifact resource trees beside `image_executable`
/// and returns the snapshot directory to hand to the daemon.
///
/// An artifact without resource trees still receives an empty snapshot, so a
/// daemon never falls back to reading files from another location.
///
/// # Errors
///
/// Returns an error if:
/// * a resource tree cannot be read, contains too many files, or nests too deeply
/// * a resource file changes while it is being snapshotted
/// * the snapshot cannot be written or published
pub fn publish_resource_snapshot(
    state_dir: &Path,
    image_executable: &Path,
) -> Result<PathBuf, DaemonLifecycleError> {
    let manifest = match artifact_resource_root() {
        Some(root) => collect_manifest(&root)?,
        None => ResourceManifest::new(),
    };
    let image_dir = image_executable
        .parent()
        .expect("content-addressed daemon image has a parent");
    let target = image_dir.join(format!("resources-{}", manifest_digest(&manifest)));
    if target.join(COMPLETE_MARKER).is_file() {
        return Ok(target);
    }
    let _publication = crate::acquire_daemon_image_publication_lock(state_dir)?;
    if target.join(COMPLETE_MARKER).is_file() {
        return Ok(target);
    }
    let temp = image_dir.join(format!(".bcode.tmp-resources-{}", std::process::id()));
    remove_dir_if_present(&temp)?;
    let published = copy_manifest(&manifest, &temp).and_then(|()| {
        remove_dir_if_present(&target)?;
        fs::rename(&temp, &target).map_err(|source| io_error(&target, source))
    });
    if published.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    published.map(|()| target)
}

/// Pins the artifact's resource trees with the daemon image in the default state
/// directory, so a daemon never reads plugins or runtimes from a mutable install.
///
/// # Errors
///
/// Returns the same errors as [`publish_resource_snapshot`].
pub fn publish_daemon_resources(image_executable: &Path) -> Result<PathBuf, DaemonLifecycleError> {
    publish_resource_snapshot(&bcode_config::default_state_dir(), image_executable)
}

fn collect_manifest(root: &Path) -> Result<ResourceManifest, DaemonLifecycleError> {
    let mut manifest = ResourceManifest::new();
    for tree in ARTIFACT_RESOURCE_TREES {
        let path = root.join(tree);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                visit(&path, tree, 0, &mut manifest)?;
            }
            Ok(_) => {
                return Err(io_error(
                    &path,
                    std::io::Error::other("artifact resource tree is not a directory"),
                ));
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Ok(manifest)
}

/// Walks one tree, following symlinks so the snapshot holds the bytes they name.
fn visit(
    directory: &Path,
    relative: &str,
    depth: usize,
    manifest: &mut ResourceManifest,
) -> Result<(), DaemonLifecycleError> {
    if depth > MAX_DEPTH {
        return Err(io_error(
            directory,
            std::io::Error::other("artifact resource tree nests too deeply"),
        ));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|source| io_error(directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(directory, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| {
            io_error(
                &path,
                std::io::Error::other("artifact resource name is not UTF-8"),
            )
        })?;
        let child = format!("{relative}/{name}");
        let metadata = fs::metadata(&path).map_err(|source| io_error(&path, source))?;
        if metadata.is_dir() {
            visit(&path, &child, depth + 1, manifest)?;
        } else if metadata.is_file() {
            if manifest.len() >= MAX_FILES {
                return Err(io_error(
                    &path,
                    std::io::Error::other("artifact resource trees contain too many files"),
                ));
            }
            let (bytes, sha256) = hash_file(&path)?;
            manifest.insert(
                child,
                ResourceFile {
                    source: path,
                    bytes,
                    sha256,
                    executable: is_executable(&metadata),
                },
            );
        }
    }
    Ok(())
}

fn manifest_digest(manifest: &ResourceManifest) -> String {
    let mut hasher = Sha256::new();
    for (relative, file) in manifest {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(file.bytes.to_le_bytes());
        hasher.update(file.sha256.as_bytes());
        hasher.update([u8::from(file.executable)]);
    }
    hex::encode(hasher.finalize())
}

/// Copies every file, re-hashing as it writes so a file that changed after
/// the manifest was taken fails publication instead of being pinned.
fn copy_manifest(manifest: &ResourceManifest, temp: &Path) -> Result<(), DaemonLifecycleError> {
    fs::create_dir_all(temp).map_err(|source| io_error(temp, source))?;
    for (relative, file) in manifest {
        let destination = relative
            .split('/')
            .fold(temp.to_path_buf(), |path, part| path.join(part));
        let parent = destination
            .parent()
            .expect("resource destination has a parent");
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        let mut input =
            fs::File::open(&file.source).map_err(|source| io_error(&file.source, source))?;
        let mut output =
            fs::File::create(&destination).map_err(|source| io_error(&destination, source))?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = vec![0_u8; 256 * 1024].into_boxed_slice();
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|source| io_error(&file.source, source))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|source| io_error(&destination, source))?;
            hasher.update(&buffer[..read]);
            copied += read as u64;
        }
        output
            .sync_all()
            .map_err(|source| io_error(&destination, source))?;
        if copied != file.bytes || hex::encode(hasher.finalize()) != file.sha256 {
            return Err(io_error(
                &file.source,
                std::io::Error::other("artifact resource changed while it was snapshotted"),
            ));
        }
        set_executable(&destination, file.executable)?;
    }
    let marker = temp.join(COMPLETE_MARKER);
    fs::write(&marker, manifest_digest(manifest)).map_err(|source| io_error(&marker, source))
}

fn hash_file(path: &Path) -> Result<(u64, String), DaemonLifecycleError> {
    let mut file = fs::File::open(path).map_err(|source| io_error(path, source))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 256 * 1024].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
const fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), DaemonLifecycleError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
const fn set_executable(_path: &Path, _executable: bool) -> Result<(), DaemonLifecycleError> {
    Ok(())
}

fn remove_dir_if_present(path: &Path) -> Result<(), DaemonLifecycleError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> DaemonLifecycleError {
    DaemonLifecycleError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::{COMPLETE_MARKER, collect_manifest, copy_manifest, manifest_digest};
    use std::fs;

    fn write(root: &std::path::Path, relative: &str, contents: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(path, contents).expect("file");
    }

    #[test]
    fn snapshot_copies_only_declared_trees_and_pins_their_bytes() {
        let install = tempfile::tempdir().expect("install");
        write(install.path(), "bcode", b"executable");
        write(install.path(), "unrelated/data.bin", b"ignored");
        write(
            install.path(),
            "bcode-runtimes/tesseract/5.5.2/lib/libtesseract.so",
            b"runtime",
        );
        write(install.path(), "plugins/example/plugin.toml", b"id = 'x'");

        let manifest = collect_manifest(install.path()).expect("manifest");
        assert_eq!(
            manifest.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "bcode-runtimes/tesseract/5.5.2/lib/libtesseract.so",
                "plugins/example/plugin.toml",
            ]
        );

        let image = tempfile::tempdir().expect("image");
        let snapshot = image.path().join("snapshot");
        copy_manifest(&manifest, &snapshot).expect("copy");
        assert!(snapshot.join(COMPLETE_MARKER).is_file());
        assert_eq!(
            fs::read(snapshot.join("plugins/example/plugin.toml")).expect("snapshot plugin"),
            b"id = 'x'"
        );

        // The snapshot is unaffected when the installed tree changes or disappears.
        fs::remove_dir_all(install.path().join("plugins")).expect("remove plugins");
        assert_eq!(
            fs::read(snapshot.join("bcode-runtimes/tesseract/5.5.2/lib/libtesseract.so"))
                .expect("snapshot runtime"),
            b"runtime"
        );
    }

    #[test]
    fn digest_changes_with_contents_and_is_stable_otherwise() {
        let install = tempfile::tempdir().expect("install");
        write(install.path(), "plugins/a/plugin.toml", b"one");
        let first = manifest_digest(&collect_manifest(install.path()).expect("manifest"));
        assert_eq!(
            first,
            manifest_digest(&collect_manifest(install.path()).expect("manifest"))
        );
        write(install.path(), "plugins/a/plugin.toml", b"two");
        assert_ne!(
            first,
            manifest_digest(&collect_manifest(install.path()).expect("manifest"))
        );
    }

    #[test]
    fn copy_rejects_a_file_changed_after_the_manifest() {
        let install = tempfile::tempdir().expect("install");
        write(install.path(), "plugins/a/plugin.toml", b"original");
        let manifest = collect_manifest(install.path()).expect("manifest");
        write(install.path(), "plugins/a/plugin.toml", b"replaced!");
        let image = tempfile::tempdir().expect("image");
        assert!(copy_manifest(&manifest, &image.path().join("snapshot")).is_err());
    }

    #[test]
    fn missing_trees_produce_an_empty_manifest() {
        let install = tempfile::tempdir().expect("install");
        write(install.path(), "bcode", b"executable");
        assert!(
            collect_manifest(install.path())
                .expect("manifest")
                .is_empty()
        );
    }
}
