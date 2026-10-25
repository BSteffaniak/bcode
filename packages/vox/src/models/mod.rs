//! Explicit, pinned GGUF acquisition. Nothing here downloads implicitly during recognition.
//!
//! Installation requires an exact catalog ID, checks every byte against the catalog digest,
//! and publishes only a complete file. Listing is metadata-only, not an integrity assertion.
//! Network cancellation is checked between reads and requests; an in-flight blocking request
//! has a 15-second timeout. Linux storage uses descriptor-relative, no-follow operations.

use crate::contracts::{Cancellation, Error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
mod storage;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod storage;

/// Immutable published model and its license/attribution information.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelEntry {
    /// Exact CLI/catalog identity; never a filesystem path.
    pub id: &'static str,
    /// Human-readable model name.
    pub name: &'static str,
    /// Local basename, fixed by the catalog.
    pub filename: &'static str,
    /// HTTPS download pinned to a repository commit, not a branch.
    pub url: &'static str,
    /// Published repository revision containing this artifact.
    pub revision: &'static str,
    /// SHA-256 of the full GGUF bytes.
    pub sha256: &'static str,
    /// Exact expected size (also the hard download/read bound).
    pub size_bytes: u64,
    /// SPDX license declared by the publisher.
    pub license: &'static str,
    /// License terms, which apply independently of Vox's own license.
    pub license_url: &'static str,
    /// Original model card for attribution and usage documentation.
    pub source_url: &'static str,
    /// Attribution retained in the built-in catalog, including conversion provenance.
    pub attribution: &'static str,
}

/// Metadata-only state; `Present` does not mean the digest was verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    /// No installed file.
    Missing,
    /// A regular private file has the catalog size; use `verify` for integrity.
    Present,
    /// Unsafe file type/ownership/permissions or wrong size. Never used implicitly.
    Rejected,
}

/// Catalog entry with its local metadata-only installation state.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelStatus {
    /// Pinned catalog entry.
    pub entry: ModelEntry,
    /// Expected installed path, even when missing.
    pub path: PathBuf,
    /// Metadata-only state.
    pub state: ModelState,
}

/// Return the complete bounded catalog without network or filesystem access.
#[must_use]
pub fn catalog() -> Vec<ModelEntry> {
    // Published revision, size and LFS SHA-256 come from the public Hugging Face
    // /api/models/handy-computer/{repository}?blobs=true metadata.
    vec![
        ModelEntry {
            id: "tiny.en",
            name: "Whisper tiny.en Q8_0",
            filename: "whisper-tiny.en-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-tiny.en-gguf/resolve/9dd46e00ef0577e354e658c07b3ac5580e9cd211/whisper-tiny.en-Q8_0.gguf",
            revision: "9dd46e00ef0577e354e658c07b3ac5580e9cd211",
            sha256: "e8c9b73c06344307d8b346e07fbe93dd88d894627854bcff31523f1ce44394fa",
            size_bytes: 45_904_544,
            license: "Apache-2.0",
            license_url: "https://www.apache.org/licenses/LICENSE-2.0",
            source_url: "https://huggingface.co/openai/whisper-tiny.en",
            attribution: "OpenAI Whisper tiny.en; GGUF conversion and Q8_0 quantization by handy-computer/transcribe.cpp",
        },
        ModelEntry {
            id: "small.en",
            name: "Whisper small.en Q8_0",
            filename: "whisper-small.en-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/whisper-small.en-gguf/resolve/ab86448a84fcc15a96a9a2216d93a8dac8427da3/whisper-small.en-Q8_0.gguf",
            revision: "ab86448a84fcc15a96a9a2216d93a8dac8427da3",
            sha256: "9614e6b7fda2d26018e4f268aece8ca25a83296ea0b534169a585b740bfd71ef",
            size_bytes: 269_674_144,
            license: "Apache-2.0",
            license_url: "https://www.apache.org/licenses/LICENSE-2.0",
            source_url: "https://huggingface.co/openai/whisper-small.en",
            attribution: "OpenAI Whisper small.en; GGUF conversion and Q8_0 quantization by handy-computer/transcribe.cpp",
        },
        ModelEntry {
            id: "parakeet-v2",
            name: "Parakeet TDT 0.6b v2 Q8_0",
            filename: "parakeet-tdt-0.6b-v2-Q8_0.gguf",
            url: "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v2-gguf/resolve/58239cf6d0ecb0af921e7936c1e5e0a8dba91dbb/parakeet-tdt-0.6b-v2-Q8_0.gguf",
            revision: "58239cf6d0ecb0af921e7936c1e5e0a8dba91dbb",
            sha256: "f0d0e99cebb6d3b83f1f7069b82b5d3c2e39a54545b0da039cb4bafd9c4e5caa",
            size_bytes: 729_574_912,
            license: "CC-BY-4.0",
            license_url: "https://creativecommons.org/licenses/by/4.0/",
            source_url: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2",
            attribution: "NVIDIA Parakeet TDT 0.6b v2; GGUF conversion and Q8_0 quantization by handy-computer/transcribe.cpp; modified from original weights",
        },
    ]
}

fn entry(id: &str) -> Result<ModelEntry> {
    catalog()
        .into_iter()
        .find(|model| model.id == id)
        .ok_or_else(|| Error::Invalid("unknown model ID; use the model catalog".into()))
}

/// Resolve the model directory, without creating it or accepting relative roots.
///
/// Uses `$XDG_DATA_HOME/vox/models`, or `$HOME/.local/share/vox/models`.
/// # Errors
/// Returns invalid input for missing/nonabsolute roots or traversal components.
pub fn data_dir() -> Result<PathBuf> {
    resolve_data_dir(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn resolve_data_dir(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    let base = if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        PathBuf::from(xdg)
    } else {
        PathBuf::from(
            home.filter(|value| !value.is_empty())
                .ok_or_else(|| Error::Invalid("set an absolute XDG_DATA_HOME or HOME".into()))?,
        )
        .join(".local/share")
    };
    if !base.is_absolute()
        || base
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(Error::Invalid(
            "model data root must be absolute without traversal".into(),
        ));
    }
    Ok(base.join("vox/models"))
}

/// List only known catalog entries. Does not hash, download, or create directories.
/// # Errors
/// Returns invalid input for an unresolved/invalid data root or unsafe storage,
/// IO errors for inaccessible directories or unreadable file metadata, and
/// unsupported on platforms without a secure storage adapter.
pub fn list() -> Result<Vec<ModelStatus>> {
    list_at(&data_dir()?)
}

fn list_at(root: &Path) -> Result<Vec<ModelStatus>> {
    let directory = match storage::Directory::open(root, false) {
        Ok(directory) => Some(directory),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    catalog()
        .into_iter()
        .map(|model| {
            let state = if let Some(directory) = &directory {
                match directory.read(model.filename) {
                    Ok(file) if file.metadata()?.len() == model.size_bytes => ModelState::Present,
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        ModelState::Missing
                    }
                    Ok(_) | Err(_) => ModelState::Rejected,
                }
            } else {
                ModelState::Missing
            };
            Ok(ModelStatus {
                path: root.join(model.filename),
                entry: model,
                state,
            })
        })
        .collect()
}

/// Explicitly download, verify, and atomically install one catalog model.
///
/// An existing file is verified and reused, never silently replaced. Remove a corrupt
/// installation explicitly first. No implicit fallback, transcript logging, or credentials.
/// # Errors
/// Returns invalid input for an unknown ID, invalid data root, unsafe storage or
/// integrity mismatch; cancelled when cancellation precedes publication; busy
/// when another operation holds the storage lock; backend errors for transport,
/// range, size or timeout failures; and IO errors for storage failures.
/// Unsupported platforms return unsupported. A directory-sync failure after
/// publication can leave a complete installed file despite returning an error.
pub fn install(id: &str, cancel: &Cancellation) -> Result<PathBuf> {
    let model = entry(id)?;
    cancel.check()?;
    install_at(&data_dir()?, &model, cancel, download)
}

fn install_at(
    root: &Path,
    model: &ModelEntry,
    cancel: &Cancellation,
    fetch: impl FnOnce(&ModelEntry, &mut File, &Cancellation) -> Result<()>,
) -> Result<PathBuf> {
    cancel.check()?;
    let directory = storage::Directory::open(root, true)?;
    directory.lock()?;
    match directory.read(model.filename) {
        Ok(mut file) => {
            verify_file(&mut file, model, cancel)?;
            return Ok(root.join(model.filename));
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut temporary = Temporary::create(&directory)?;
    fetch(model, &mut temporary.file, cancel)?;
    cancel.check()?;
    verify_file(&mut temporary.file, model, cancel)?;
    temporary.file.sync_all()?;
    cancel.check()?;
    directory.publish(&temporary.name, model.filename)?;
    // Publication is the commit point: cancellation afterwards does not report an
    // unpublished outcome. A directory-sync error can leave a valid published file.
    directory.sync()?;
    Ok(root.join(model.filename))
}

/// Verify exact size, GGUF signature and SHA-256 of an installed catalog entry.
/// # Errors
/// Returns invalid input for an unknown ID, invalid data root, unsafe storage or
/// integrity mismatch; busy when another operation holds the storage lock; IO
/// errors for missing or unreadable files; and unsupported on platforms without
/// a secure storage adapter.
pub fn verify(id: &str) -> Result<PathBuf> {
    let model = entry(id)?;
    verify_at(&data_dir()?, &model, &Cancellation::new())
}

/// Cancellable form of `verify` for embedding frontends.
/// # Errors
/// Returns all errors documented by [`verify`], plus cancelled when the supplied
/// signal is observed before or during verification.
pub fn verify_with_cancel(id: &str, cancel: &Cancellation) -> Result<PathBuf> {
    let model = entry(id)?;
    cancel.check()?;
    verify_at(&data_dir()?, &model, cancel)
}

fn verify_at(root: &Path, model: &ModelEntry, cancel: &Cancellation) -> Result<PathBuf> {
    cancel.check()?;
    let directory = storage::Directory::open(root, false)?;
    directory.lock()?;
    verify_file(&mut directory.read(model.filename)?, model, cancel)?;
    Ok(root.join(model.filename))
}

/// Remove exactly one regular catalog file; missing files are an idempotent success.
/// Never accepts a path, follows a symlink, or recursively deletes anything.
/// # Errors
/// Returns invalid input for an unknown ID, invalid data root or unsafe storage;
/// busy when another operation holds the storage lock; IO errors for removal or
/// directory-sync failures; and unsupported on platforms without a secure storage
/// adapter. A sync failure can occur after the model has already been removed.
pub fn remove(id: &str) -> Result<()> {
    let model = entry(id)?;
    remove_at(&data_dir()?, &model)
}

fn remove_at(root: &Path, model: &ModelEntry) -> Result<()> {
    let directory = match storage::Directory::open(root, false) {
        Ok(directory) => directory,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    directory.lock()?;
    match directory.read(model.filename) {
        Ok(_) => directory.unlink(model.filename)?,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    directory.sync()
}

fn integrity_error() -> Error {
    Error::Invalid("model integrity mismatch (GGUF, exact size or SHA-256); remove explicitly before reinstalling".into())
}

fn verify_file(file: &mut File, model: &ModelEntry, cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    if file.metadata()?.len() != model.size_bytes || model.size_bytes < 4 {
        return Err(integrity_error());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(integrity_error());
    }
    let mut hash = Sha256::new();
    hash.update(magic);
    let mut remaining = model.size_bytes - 4;
    let mut buffer = vec![0; 64 * 1024].into_boxed_slice();
    while remaining > 0 {
        cancel.check()?;
        let limit =
            usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| integrity_error())?;
        let count = file.read(&mut buffer[..limit])?;
        if count == 0 {
            return Err(integrity_error());
        }
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    cancel.check()?;
    if file.read(&mut [0; 1])? != 0 || hex::encode(hash.finalize()) != model.sha256 {
        return Err(integrity_error());
    }
    Ok(())
}

fn network_error() -> Error {
    // Do not propagate reqwest errors: they can embed URLs and proxy credentials.
    Error::Backend(
        "model download failed; retry the explicit install when connectivity is available".into(),
    )
}

fn download(model: &ModelEntry, file: &mut File, cancel: &Cancellation) -> Result<()> {
    use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, RANGE};
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()
        .map_err(|_| network_error())?;
    let started = Instant::now();
    let mut offset = 0;
    while offset < model.size_bytes {
        cancel.check()?;
        if started.elapsed() > Duration::from_mins(30) {
            return Err(Error::Backend(
                "model download exceeded the 30-minute limit".into(),
            ));
        }
        let end = (offset + 4 * 1024 * 1024 - 1).min(model.size_bytes - 1);
        let response = client
            .get(model.url)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, format!("bytes={offset}-{end}"))
            .send();
        cancel.check()?;
        let mut response = response.map_err(|_| network_error())?;
        let expected_range = format!("bytes {offset}-{end}/{}", model.size_bytes);
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            || response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                != Some(expected_range.as_str())
            || response
                .headers()
                .get(CONTENT_ENCODING)
                .is_some_and(|value| value != "identity")
            || response
                .content_length()
                .is_some_and(|size| size != end - offset + 1)
        {
            return Err(Error::Backend(
                "model server returned an invalid range, length or encoding".into(),
            ));
        }
        copy_exact_bounded(&mut response, file, end - offset + 1, cancel)?;
        offset = end + 1;
    }
    Ok(())
}

fn copy_exact_bounded(
    source: &mut impl Read,
    target: &mut impl Write,
    expected: u64,
    cancel: &Cancellation,
) -> Result<()> {
    let mut remaining = expected;
    let mut buffer = vec![0; 64 * 1024].into_boxed_slice();
    while remaining > 0 {
        cancel.check()?;
        let limit =
            usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| network_error())?;
        let read = source.read(&mut buffer[..limit]);
        cancel.check()?;
        let count = read.map_err(|_| network_error())?;
        if count == 0 {
            return Err(Error::Backend("model download was truncated".into()));
        }
        target.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    let read = source.read(&mut [0; 1]);
    cancel.check()?;
    if read.map_err(|_| network_error())? != 0 {
        return Err(Error::Backend(
            "model download exceeded the catalog size".into(),
        ));
    }
    Ok(())
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Temporary<'a> {
    directory: &'a storage::Directory,
    name: String,
    file: File,
}

impl<'a> Temporary<'a> {
    fn create(directory: &'a storage::Directory) -> Result<Self> {
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(".download-{}-{sequence}.part", std::process::id());
            match directory.create(&name) {
                Ok(file) => {
                    return Ok(Self {
                        directory,
                        name,
                        file,
                    });
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::Busy)
    }
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        // Only this attempt's private basename; never wildcard or recursive cleanup.
        let _ = self.directory.unlink(&self.name);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
