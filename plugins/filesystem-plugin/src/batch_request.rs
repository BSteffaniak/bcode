//! Bounded validation of the portable multi-edit request before target access.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

const MAX_FILES: usize = 64;
const MAX_EDITS: usize = 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;

/// Filesystem-owned preparation envelope. Consumers must reject versions other
/// than 1 before interpreting targets; this is not a durable mutation receipt.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedBatch {
    #[serde(deserialize_with = "deserialize_version")]
    pub version: u32,
    pub request_digest: [u8; 32],
    #[serde(deserialize_with = "deserialize_targets")]
    pub targets: Vec<PreparedTarget>,
}

fn deserialize_targets<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<PreparedTarget>, D::Error> {
    struct Targets;
    impl<'de> serde::de::Visitor<'de> for Targets {
        type Value = Vec<PreparedTarget>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "1..={MAX_FILES} prepared targets")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut targets = Vec::new();
            let mut paths = BTreeSet::new();
            let mut identities = BTreeSet::new();
            while let Some(target) = sequence.next_element::<PreparedTarget>()? {
                if targets.len() == MAX_FILES {
                    return Err(serde::de::Error::custom("too many prepared targets"));
                }
                if !target.path.is_absolute()
                    || target.path.file_name().is_none()
                    || target
                        .path
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                    || target
                        .path
                        .to_str()
                        .is_none_or(|path| path.len() > MAX_PATH_BYTES || path.contains('\0'))
                {
                    return Err(serde::de::Error::custom("invalid prepared target path"));
                }
                if !paths.insert(target.path.clone())
                    || !identities.insert((target.device, target.inode))
                {
                    return Err(serde::de::Error::custom("duplicate prepared target"));
                }
                targets.push(target);
            }
            if targets.is_empty() {
                return Err(serde::de::Error::custom(
                    "prepared targets must not be empty",
                ));
            }
            Ok(targets)
        }
    }
    deserializer.deserialize_seq(Targets)
}

fn deserialize_version<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let version = <u32 as serde::Deserialize>::deserialize(deserializer)?;
    if version != 1 {
        return Err(serde::de::Error::custom(
            "unsupported multi-edit preparation version",
        ));
    }
    Ok(version)
}

/// Canonical target identity captured from a no-follow open during preparation.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedTarget {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub parent_device: u64,
    pub parent_inode: u64,
}

/// Validate the already-decoded request before resolving any target paths.
/// Transport-level JSON allocation limits remain the invocation host's concern.
pub fn prepare(arguments: &Value, root: Option<&Path>) -> Result<PreparedBatch, String> {
    validate(arguments)?;
    let files = arguments["files"].as_array().expect("validated files");
    let mut seen = BTreeSet::new();
    let mut identities = BTreeSet::new();
    let mut paths = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let path = Path::new(file["path"].as_str().expect("validated path"));
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.ok_or_else(|| "relative paths require workspace host context".to_owned())?
                .join(path)
        };
        let canonical = absolute
            .canonicalize()
            .map_err(|error| format!("file {}: {error}", index + 1))?;
        if canonical
            .to_str()
            .is_none_or(|path| path.len() > MAX_PATH_BYTES)
        {
            return Err(format!(
                "file {}: canonical target is not UTF-8 or exceeds the path budget and cannot be represented in policy facts",
                index + 1
            ));
        }
        if !seen.insert(canonical.clone()) {
            return Err(format!("file {}: duplicate canonical target", index + 1));
        }
        let target =
            validate_target(&canonical).map_err(|error| format!("file {}: {error}", index + 1))?;
        if !identities.insert((target.device, target.inode)) {
            return Err(format!("file {}: duplicate file identity", index + 1));
        }
        paths.push(target);
    }
    Ok(PreparedBatch {
        version: 1,
        request_digest: request_digest(arguments),
        targets: paths,
    })
}

// Hash validated semantic fields rather than JSON formatting or map ordering.
fn request_digest(arguments: &Value) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut digest = Sha256::new();
    for file in arguments["files"].as_array().expect("validated files") {
        let edits = file["edits"].as_array().expect("validated edits");
        digest.update((edits.len() as u64).to_le_bytes());
        for text in std::iter::once(file["path"].as_str().expect("validated path")).chain(
            edits.iter().flat_map(|edit| {
                [
                    edit["old_text"].as_str().expect("validated search"),
                    edit["new_text"].as_str().expect("validated replacement"),
                ]
            }),
        ) {
            digest.update((text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
        }
    }
    digest.finalize().into()
}

/// Verify request binding and reopen prepared identities without mutating files.
/// Publication must still recheck identities and snapshots under coordination.
#[cfg(all(test, unix))]
pub fn verify(arguments: &Value, descriptor: &PreparedBatch) -> Result<(), String> {
    verify_cancellable(arguments, descriptor, &|| false)
}

/// Screen a batch while observing cancellation between bounded read chunks.
#[cfg(all(test, unix))]
pub fn verify_cancellable(
    arguments: &Value,
    descriptor: &PreparedBatch,
    cancelled: &impl Fn() -> bool,
) -> Result<(), String> {
    collect_snapshots(arguments, descriptor, cancelled).map(|_| ())
}

fn collect_snapshots(
    arguments: &Value,
    descriptor: &PreparedBatch,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<Snapshot>, String> {
    check_cancelled(cancelled)?;
    validate(arguments)?;
    if descriptor.version != 1
        || descriptor.request_digest != request_digest(arguments)
        || descriptor.targets.len()
            != arguments["files"]
                .as_array()
                .expect("validated files")
                .len()
    {
        return Err("multi-edit request does not match its preparation".to_owned());
    }
    let mut snapshots = Vec::with_capacity(descriptor.targets.len());
    let mut remaining_bytes = 16 * 1024 * 1024;
    for (index, target) in descriptor.targets.iter().enumerate() {
        let current = validate_target(&target.path)
            .map_err(|error| format!("file {}: {error}", index + 1))?;
        if current.device != target.device
            || current.inode != target.inode
            || current.parent_device != target.parent_device
            || current.parent_inode != target.parent_inode
        {
            return Err(format!(
                "file {}: prepared target identity changed",
                index + 1
            ));
        }
        check_cancelled(cancelled)?;
        snapshots.push(
            validate_snapshot(
                &arguments["files"][index],
                target,
                &mut remaining_bytes,
                cancelled,
            )
            .map_err(|error| format!("file {}: {error}", index + 1))?,
        );
    }
    for (index, (snapshot, target)) in snapshots.iter().zip(&descriptor.targets).enumerate() {
        snapshot
            .recheck(target, cancelled)
            .map_err(|error| format!("file {}: {error}", index + 1))?;
    }
    Ok(snapshots)
}

/// Execute an authorized batch while the caller holds mutation coordination.
/// Validation failures publish nothing; later failures never undo prior commits.
#[cfg(all(test, unix))]
pub fn execute(
    arguments: &Value,
    descriptor: &PreparedBatch,
    cancelled: &impl Fn() -> bool,
) -> Result<Value, String> {
    execute_with_changes(arguments, descriptor, cancelled, &mut |_, _, _| Value::Null)
}

/// Retain omitted committed changes through the caller's bounded artifact boundary.
/// Retention failure must not change publication outcomes or trigger a retry.
pub fn execute_with_changes(
    arguments: &Value,
    descriptor: &PreparedBatch,
    cancelled: &impl Fn() -> bool,
    retain: &mut impl FnMut(usize, &str, &str) -> Value,
) -> Result<Value, String> {
    let snapshots = collect_snapshots(arguments, descriptor, cancelled)?;
    let mut stopped = false;
    let mut outcomes = Vec::with_capacity(snapshots.len());
    let mut change_bytes_remaining = 64 * 1024;
    for (index, (snapshot, target)) in snapshots.iter().zip(&descriptor.targets).enumerate() {
        let (status, error) = if stopped {
            ("not_attempted", None)
        } else if cancelled() {
            stopped = true;
            ("cancelled", None)
        } else {
            match snapshot.publish(target, cancelled) {
                #[cfg(unix)]
                Ok(PublicationOutcome::Committed) => ("committed", None),
                Ok(PublicationOutcome::Unchanged) => ("unchanged", None),
                #[cfg(unix)]
                Ok(PublicationOutcome::Unknown(error)) => {
                    stopped = true;
                    ("unknown", Some(error))
                }
                Err(error) => {
                    stopped = true;
                    (
                        if cancelled() { "cancelled" } else { "failed" },
                        Some(error),
                    )
                }
            }
        };
        let mut change = if status == "committed" {
            bounded_change(
                &snapshot.source,
                &snapshot.output,
                &mut change_bytes_remaining,
            )
        } else {
            Value::Null
        };
        if change["omitted"] == true {
            change["retained"] = retain(index, &snapshot.source, &snapshot.output);
        }
        outcomes.push(serde_json::json!({"path": target.path, "status": status, "error": error, "change": change}));
    }
    Ok(serde_json::json!({"version": 1, "is_error": stopped, "files": outcomes}))
}

/// Produce an exact whole-file unified diff in linear time and bounded space.
/// Fixed labels avoid interpreting untrusted paths as patch headers. This is a
/// presentation artifact, not an alternate mutation interface or a minimal diff.
pub fn retained_diff(source: &str, output: &str) -> String {
    use std::fmt::Write as _;
    let old_lines = source.split_inclusive('\n').count();
    let new_lines = output.split_inclusive('\n').count();
    let mut diff = String::new();
    let _ = writeln!(
        diff,
        "--- before\n+++ after\n@@ -{},{} +{},{} @@",
        usize::from(old_lines != 0),
        old_lines,
        usize::from(new_lines != 0),
        new_lines
    );
    for (prefix, text) in [('-', source), ('+', output)] {
        for line in text.split_inclusive('\n') {
            diff.push(prefix);
            diff.push_str(line);
            if !line.ends_with('\n') {
                diff.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    diff
}

/// Exact changed region after excluding identical whole-line prefixes/suffixes.
/// Never truncate changed text: oversized regions retain their source artifacts.
fn bounded_change(source: &str, output: &str, remaining: &mut usize) -> Value {
    let mut prefix = 0;
    let mut start_line = 1;
    for (old, new) in source
        .split_inclusive('\n')
        .zip(output.split_inclusive('\n'))
    {
        if old != new || !old.ends_with('\n') {
            break;
        }
        prefix += old.len();
        start_line += 1;
    }
    let old_tail = &source[prefix..];
    let new_tail = &output[prefix..];
    let mut suffix = 0;
    for (old, new) in old_tail
        .split_inclusive('\n')
        .rev()
        .zip(new_tail.split_inclusive('\n').rev())
    {
        if old != new {
            break;
        }
        suffix += old.len();
    }
    let old_region = &old_tail[..old_tail.len() - suffix];
    let new_region = &new_tail[..new_tail.len() - suffix];
    let bytes = old_region.len().saturating_add(new_region.len());
    if bytes > 16 * 1024 || bytes > *remaining {
        return serde_json::json!({"omitted": true, "reason": "display_budget", "old_bytes": source.len(), "new_bytes": output.len()});
    }
    *remaining -= bytes;
    serde_json::json!({"omitted": false, "old_text": old_region, "new_text": new_region, "old_start_line": start_line, "new_start_line": start_line})
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err("multi-edit cancelled before mutation".to_owned())
    } else {
        Ok(())
    }
}

/// In-memory execution data, never serialized into session history.
struct Snapshot {
    source: String,
    output: String,
    #[cfg(unix)]
    parent: std::fs::File,
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(unix)]
    permissions: std::fs::Permissions,
    #[cfg(unix)]
    metadata: std::fs::Metadata,
}

#[cfg(unix)]
fn metadata_unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.size() == after.size()
        && before.mode() == after.mode()
        && before.uid() == after.uid()
        && before.gid() == after.gid()
        && before.nlink() == after.nlink()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

enum PublicationOutcome {
    Unchanged,
    #[cfg(unix)]
    Committed,
    #[cfg(unix)]
    Unknown(String),
}

impl Snapshot {
    fn publish(
        &self,
        target: &PreparedTarget,
        cancelled: &impl Fn() -> bool,
    ) -> Result<PublicationOutcome, String> {
        self.recheck(target, cancelled)?;
        if self.source == self.output {
            return Ok(PublicationOutcome::Unchanged);
        }
        #[cfg(unix)]
        {
            let staged = super::confined::StagedFile::create(
                &self.parent,
                self.output.as_bytes(),
                self.permissions.clone(),
                cancelled,
            )
            .map_err(|error| error.to_string())?;
            self.recheck(target, cancelled)?;
            let publication = staged
                .publish(
                    target.path.file_name().ok_or("missing target name")?,
                    self.output.as_bytes(),
                    cancelled,
                )
                .map_err(|error| error.to_string())?;
            Ok(match publication {
                super::confined::Publication::Committed => PublicationOutcome::Committed,
                super::confined::Publication::Unknown(error) => PublicationOutcome::Unknown(
                    format!("publication outcome unknown; inspect target before retrying: {error}"),
                ),
            })
        }
        #[cfg(not(unix))]
        Err("multi-edit publication unsupported on this platform".to_owned())
    }

    fn recheck(
        &self,
        target: &PreparedTarget,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), String> {
        check_cancelled(cancelled)?;
        #[cfg(unix)]
        {
            use std::io::Read as _;
            use std::os::unix::fs::MetadataExt as _;
            let current = validate_target(&target.path)?;
            let parent = self.parent.metadata().map_err(|error| error.to_string())?;
            let file = self.file.metadata().map_err(|error| error.to_string())?;
            if current.device != file.dev()
                || current.inode != file.ino()
                || current.parent_device != parent.dev()
                || current.parent_inode != parent.ino()
                || file.nlink() != 1
            {
                return Err("snapshot identity changed".to_owned());
            }
            if file.permissions() != self.permissions {
                return Err("snapshot permissions changed".to_owned());
            }
            let (reopened_parent, reopened) =
                super::confined::open_target(&target.path).map_err(|error| error.to_string())?;
            let reopened_parent = reopened_parent
                .metadata()
                .map_err(|error| error.to_string())?;
            let metadata = reopened.metadata().map_err(|error| error.to_string())?;
            if metadata.dev() != file.dev()
                || metadata.ino() != file.ino()
                || reopened_parent.dev() != parent.dev()
                || reopened_parent.ino() != parent.ino()
                || metadata.nlink() != 1
            {
                return Err("snapshot identity changed".to_owned());
            }
            let mut reader = reopened.take(self.source.len() as u64 + 1);
            let mut chunk = [0u8; 8192];
            let mut offset = 0;
            loop {
                check_cancelled(cancelled)?;
                let count = reader.read(&mut chunk).map_err(|error| error.to_string())?;
                if count == 0 {
                    break;
                }
                if self.source.as_bytes().get(offset..offset + count) != Some(&chunk[..count]) {
                    return Err("snapshot content changed".to_owned());
                }
                offset += count;
            }
            if offset != self.source.len() {
                return Err("snapshot content changed".to_owned());
            }
            let after = reader
                .get_ref()
                .metadata()
                .map_err(|error| error.to_string())?;
            if after.permissions() != self.permissions || after.nlink() != 1 {
                return Err("snapshot permissions or links changed".to_owned());
            }
            if !metadata_unchanged(&self.metadata, &after) {
                return Err("snapshot metadata changed".to_owned());
            }
            // Retain replacement output alongside the source through the full recheck.
            debug_assert!(self.output.len() <= 4 * 1024 * 1024);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (self, target);
            Err("snapshot recheck unsupported".to_owned())
        }
    }
}

// Publication must still acquire coordination before constructing these snapshots.
fn validate_snapshot(
    file: &Value,
    target: &PreparedTarget,
    remaining: &mut usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Snapshot, String> {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;
        const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
        let (parent, handle) =
            super::confined::open_target(&target.path).map_err(|error| error.to_string())?;
        let metadata = handle.metadata().map_err(|error| error.to_string())?;
        let parent_metadata = parent.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.dev() != target.device
            || metadata.ino() != target.inode
            || parent_metadata.dev() != target.parent_device
            || parent_metadata.ino() != target.parent_inode
        {
            return Err("prepared target identity changed".to_owned());
        }
        let limit = MAX_FILE_BYTES.min(*remaining);
        let mut source = Vec::new();
        let mut reader = handle.take(limit as u64 + 1);
        let mut chunk = [0u8; 8 * 1024];
        loop {
            check_cancelled(cancelled)?;
            let read = reader.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            let required = source.len() + read;
            if required > source.capacity() {
                source
                    .try_reserve_exact(required - source.len())
                    .map_err(|_| "cannot allocate bounded snapshot".to_owned())?;
            }
            if source.capacity() > limit + 1 {
                return Err("snapshot allocation exceeds byte budget".to_owned());
            }
            source.extend_from_slice(&chunk[..read]);
        }
        let after = reader
            .get_ref()
            .metadata()
            .map_err(|error| error.to_string())?;
        if !metadata_unchanged(&metadata, &after) {
            return Err("snapshot metadata changed during read".to_owned());
        }
        let source = String::from_utf8(source)
            .map_err(|error| format!("cannot read UTF-8 snapshot: {error}"))?;
        if source.len() > limit {
            return Err("snapshot exceeds file or aggregate byte budget".to_owned());
        }
        *remaining = remaining
            .checked_sub(source.capacity())
            .ok_or("snapshot allocation exceeds aggregate byte budget")?;
        let edits = file["edits"]
            .as_array()
            .expect("validated edits")
            .iter()
            .map(|edit| {
                (
                    edit["old_text"].as_str().expect("validated search"),
                    edit["new_text"].as_str().expect("validated replacement"),
                )
            })
            .collect::<Vec<_>>();
        let (output, _) = super::replacement::replace_snapshot_cancellable(
            &source,
            &edits,
            MAX_FILE_BYTES.min(*remaining),
            false,
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        *remaining = remaining
            .checked_sub(output.capacity())
            .ok_or("replacement allocation exceeds aggregate byte budget")?;
        Ok(Snapshot {
            source,
            output,
            parent,
            file: reader.into_inner(),
            permissions: metadata.permissions(),
            metadata,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (file, target, remaining, cancelled);
        Err("multi-edit snapshots are unsupported on this platform".to_owned())
    }
}

// Preparation screens metadata through a no-follow handle. Execution must
// independently reopen and compare authorized identities before publication.
fn validate_target(path: &Path) -> Result<PreparedTarget, String> {
    #[cfg(unix)]
    let (parent_metadata, metadata) = super::confined::open_target(path)
        .and_then(|(parent, file)| Ok((parent.metadata()?, file.metadata()?)))
        .map_err(|error| error.to_string())?;
    #[cfg(not(unix))]
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("multi-edit requires a regular file".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("multi-edit does not support hard-linked targets".to_owned());
        }
        Ok(PreparedTarget {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            parent_device: parent_metadata.dev(),
            parent_inode: parent_metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Err("multi-edit target identity checks are unsupported on this platform".to_owned())
    }
}

fn validate(arguments: &Value) -> Result<(), String> {
    let object = arguments
        .as_object()
        .ok_or("multi-edit arguments must be an object")?;
    if object.len() != 1 {
        return Err("multi-edit accepts only files".to_owned());
    }
    let files = arguments["files"]
        .as_array()
        .ok_or("files must be an array")?;
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(format!("files must contain 1..={MAX_FILES} entries"));
    }
    let mut edit_count = 0usize;
    let mut text_bytes = 0usize;
    for (file_index, file) in files.iter().enumerate() {
        let label = format!("file {}", file_index + 1);
        let object = file
            .as_object()
            .ok_or_else(|| format!("{label}: expected an object"))?;
        if object.len() != 2 || !object.contains_key("path") || !object.contains_key("edits") {
            return Err(format!("{label}: expected only path and edits"));
        }
        let path = file["path"]
            .as_str()
            .ok_or_else(|| format!("{label}: path must be a string"))?;
        if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
            return Err(format!(
                "{label}: path must contain 1..={MAX_PATH_BYTES} bytes without NUL"
            ));
        }
        let edits = file["edits"]
            .as_array()
            .ok_or_else(|| format!("{label}: edits must be an array"))?;
        if edits.is_empty() || edits.len() > MAX_EDITS - edit_count {
            return Err(format!(
                "{label}: nonempty edits required; batch limit is {MAX_EDITS}"
            ));
        }
        edit_count += edits.len();
        for (edit_index, edit) in edits.iter().enumerate() {
            let label = format!("{label}, edit {}", edit_index + 1);
            let object = edit
                .as_object()
                .ok_or_else(|| format!("{label}: expected an object"))?;
            if object.len() != 2
                || !object.contains_key("old_text")
                || !object.contains_key("new_text")
            {
                return Err(format!("{label}: expected only old_text and new_text"));
            }
            for key in ["old_text", "new_text"] {
                let text = edit[key]
                    .as_str()
                    .ok_or_else(|| format!("{label}: {key} must be a string"))?;
                if key == "old_text" && text.is_empty() {
                    return Err(format!("{label}: old_text must not be empty"));
                }
                if text.len() > MAX_TEXT_BYTES - text_bytes {
                    return Err(format!(
                        "{label}: batch replacement text exceeds {MAX_TEXT_BYTES} bytes"
                    ));
                }
                text_bytes += text.len();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn retained_diff_preserves_line_endings_and_missing_final_newline() {
        assert_eq!(
            super::retained_diff("a\r\n界", "b\n"),
            "--- before\n+++ after\n@@ -1,2 +1,1 @@\n-a\r\n-界\n\\ No newline at end of file\n+b\n"
        );
        assert_eq!(
            super::retained_diff("", "x"),
            "--- before\n+++ after\n@@ -0,0 +1,1 @@\n+x\n\\ No newline at end of file\n"
        );
        assert_eq!(
            super::retained_diff("x\n", ""),
            "--- before\n+++ after\n@@ -1,1 +0,0 @@\n-x\n"
        );
        let text = "\n".repeat(100_000);
        assert!(super::retained_diff(&text, &text).len() <= 2 * (text.len() + text.len()) + 128);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn compact_changes_preserve_insertions_deletions_and_final_newlines() {
        for (old, new, expected_old, expected_new, start) in [
            ("a\nb\n", "a\nx\nb\n", "", "x\n", 2),
            ("a\nx\nb\n", "a\nb\n", "x\n", "", 2),
            ("a\n尾", "a\n尾\n", "尾", "尾\n", 2),
            ("a\n尾\n", "a\n尾", "尾\n", "尾", 2),
            ("\u{feff}a\r\nx\n", "\u{feff}a\r\ny\n", "x\n", "y\n", 2),
            ("", "🙂", "", "🙂", 1),
            ("🙂", "", "🙂", "", 1),
        ] {
            let mut budget = 64 * 1024;
            let change = bounded_change(old, new, &mut budget);
            assert_eq!(change["omitted"], false);
            assert_eq!(change["old_text"], expected_old);
            assert_eq!(change["new_text"], expected_new);
            assert_eq!(change["old_start_line"], start);
            assert_eq!(change["new_start_line"], start);
            assert_eq!(budget, 64 * 1024 - expected_old.len() - expected_new.len());
        }
        let mut budget = 1;
        let change = bounded_change("same\nold\n", "same\nnew\n", &mut budget);
        assert_eq!(change["omitted"], true);
        assert_eq!(budget, 1, "omitted pairs must not consume display budget");
    }

    #[test]
    fn localized_large_file_change_keeps_exact_lines_and_offsets() {
        let prefix = "same\r\n".repeat(5000);
        let suffix = "尾\n".repeat(5000);
        let mut budget = 64 * 1024;
        let change = super::bounded_change(
            &format!("{prefix}old\r\n{suffix}"),
            &format!("{prefix}new\n{suffix}"),
            &mut budget,
        );
        assert_eq!(change["omitted"], false);
        assert_eq!(change["old_text"], "old\r\n");
        assert_eq!(change["new_text"], "new\n");
        assert_eq!(change["old_start_line"], 5001);
        assert_eq!(change["new_start_line"], 5001);
        assert_eq!(budget, 64 * 1024 - 9);
    }

    #[test]
    #[cfg(unix)]
    fn omitted_committed_changes_retain_exact_sources_without_changing_outcome() {
        let root = crate::tests::temp_dir("batch-retained-change");
        let path = root.join("large");
        let old = format!("{}needle", "unchanged".repeat(2000));
        std::fs::write(&path, &old).unwrap();
        let request = json!({"files":[{"path":path,"edits":[{"old_text":"needle","new_text":"replacement"}]}]});
        let descriptor = prepare(&request, None).unwrap();
        let mut calls = 0;
        let outcome = execute_with_changes(
            &request,
            &descriptor,
            &|| false,
            &mut |index, before, after| {
                calls += 1;
                assert_eq!(index, 0);
                assert_eq!(before, old);
                assert_eq!(after, std::fs::read_to_string(&path).unwrap());
                json!({"old":{"unavailable":true},"new":{"unavailable":true}})
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(outcome["files"][0]["status"], "committed");
        assert_eq!(outcome["is_error"], false);
        assert_eq!(
            outcome["files"][0]["change"]["retained"]["old"]["unavailable"],
            true
        );
    }

    #[cfg(unix)]
    #[test]
    fn execution_validates_whole_batch_and_accounts_for_commits_and_cancellation() {
        let root = crate::tests::temp_dir("batch-execution");
        let paths = [root.join("first"), root.join("second"), root.join("third")];
        for path in &paths {
            std::fs::write(path, "alpha beta\r\n").unwrap();
        }
        let request = json!({"files": paths.iter().map(|path| json!({
            "path": path,
            "edits": [{"old_text":"beta","new_text":"gamma"}, {"old_text":"alpha","new_text":"beta"}]
        })).collect::<Vec<_>>()});
        let mut invalid = request.clone();
        invalid["files"][2]["edits"][0]["old_text"] = json!("missing");
        let descriptor = prepare(&invalid, None).unwrap();
        assert!(execute(&invalid, &descriptor, &|| false).is_err());
        for path in &paths {
            assert_eq!(std::fs::read(path).unwrap(), b"alpha beta\r\n");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        let descriptor = prepare(&request, None).unwrap();
        let outcome = execute(&request, &descriptor, &|| {
            std::fs::read(&paths[0]).unwrap() == b"beta gamma\r\n"
        })
        .unwrap();
        assert_eq!(outcome["files"][0]["status"], "committed");
        assert_eq!(outcome["files"][1]["status"], "cancelled");
        assert_eq!(outcome["files"][2]["status"], "not_attempted");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"beta gamma\r\n");
        for path in &paths[1..] {
            assert_eq!(std::fs::read(path).unwrap(), b"alpha beta\r\n");
        }
        std::fs::write(&paths[0], "alpha beta\r\n").unwrap();
        let descriptor = prepare(&request, None).unwrap();
        let outcome = execute(&request, &descriptor, &|| false).unwrap();
        assert_eq!(outcome["is_error"], false);
        for (index, path) in paths.iter().enumerate() {
            assert_eq!(outcome["files"][index]["status"], "committed");
            assert_eq!(std::fs::read(path).unwrap(), b"beta gamma\r\n");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_later_target_stops_batch_without_undoing_committed_files() {
        let root = crate::tests::temp_dir("batch-partial-stale");
        let paths = [root.join("first"), root.join("second"), root.join("third")];
        for path in &paths {
            std::fs::write(path, b"original").unwrap();
        }
        let request = json!({"files": paths.iter().map(|path| json!({
            "path": path, "edits": [{"old_text":"original", "new_text":"replacement"}]
        })).collect::<Vec<_>>()});
        let descriptor = prepare(&request, None).unwrap();
        let changed = std::cell::Cell::new(false);
        let outcome = execute(&request, &descriptor, &|| {
            if !changed.get() && std::fs::read(&paths[0]).unwrap() == b"replacement" {
                std::fs::write(&paths[1], b"external").unwrap();
                changed.set(true);
            }
            false
        })
        .unwrap();
        assert!(changed.get());
        assert_eq!(outcome["is_error"], true);
        assert_eq!(outcome["files"][0]["status"], "committed");
        assert_eq!(outcome["files"][1]["status"], "failed");
        assert!(
            outcome["files"][1]["error"]
                .as_str()
                .unwrap()
                .contains("changed")
        );
        assert_eq!(outcome["files"][2]["status"], "not_attempted");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"external");
        assert_eq!(std::fs::read(&paths[2]).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 3);
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn change_display_budget_keeps_complete_pairs_or_explicitly_omits() {
        let mut remaining = 10;
        let change = bounded_change("旧\r\n", "新\r\n", &mut remaining);
        assert_eq!(change["old_text"], "旧\r\n");
        assert_eq!(change["new_text"], "新\r\n");
        assert_eq!(remaining, 0);
        assert_eq!(bounded_change("a", "b", &mut remaining)["omitted"], true);
        let mut remaining = 64 * 1024;
        let oversized = "x".repeat(16 * 1024 + 1);
        let omitted = bounded_change(&oversized, "", &mut remaining);
        assert_eq!(omitted["omitted"], true);
        assert!(omitted.get("old_text").is_none());
        assert_eq!(remaining, 64 * 1024);
    }

    fn prepare_paths(arguments: &Value, root: Option<&Path>) -> Result<Vec<PathBuf>, String> {
        prepare(arguments, root).map(|batch| {
            batch
                .targets
                .into_iter()
                .map(|target| target.path)
                .collect()
        })
    }

    #[cfg(unix)]
    #[test]
    fn preparation_detects_parent_replacement_even_when_file_inode_is_preserved() {
        let root = crate::tests::temp_dir("batch-parent-identity");
        let parent = root.join("parent");
        let moved = root.join("moved");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("target");
        std::fs::write(&path, "original").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"original","new_text":"new"}]}]});
        let first = prepare(&request, None).unwrap();
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::rename(moved.join("target"), &path).unwrap();
        let second = prepare(&request, None).unwrap();
        assert_eq!(first.targets[0].inode, second.targets[0].inode);
        assert_ne!(
            first.targets[0].parent_inode,
            second.targets[0].parent_inode
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::fs::remove_dir(moved).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_mutation_during_initial_read() {
        let root = crate::tests::temp_dir("batch-read-race");
        let path = root.join("file");
        std::fs::write(&path, "original").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"original","new_text":"new"}]}]});
        let prepared = prepare(&request, None).unwrap();
        let polls = std::cell::Cell::new(0);
        let result = validate_snapshot(
            &request["files"][0],
            &prepared.targets[0],
            &mut (16 * 1024 * 1024),
            &|| {
                polls.set(polls.get() + 1);
                if polls.get() == 2 {
                    // The first chunk has been read. Grow the same inode before EOF.
                    use std::io::Write as _;
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b" appended externally")
                        .unwrap();
                }
                false
            },
        );
        let Err(error) = result else {
            panic!("accepted changing snapshot");
        };
        assert!(error.contains("metadata changed during read"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original appended externally"
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_budget_accounts_for_retained_allocations() {
        let root = crate::tests::temp_dir("batch-allocation-budget");
        let path = root.join("file");
        std::fs::write(&path, "original").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"original","new_text":"new"}]}]});
        let prepared = prepare(&request, None).unwrap();
        let mut budget = 1024;
        let snapshot = validate_snapshot(
            &request["files"][0],
            &prepared.targets[0],
            &mut budget,
            &|| false,
        )
        .unwrap();
        assert_eq!(
            budget + snapshot.source.capacity() + snapshot.output.capacity(),
            1024
        );
        assert!(
            validate_snapshot(&request["files"][0], &prepared.targets[0], &mut 10, &|| {
                false
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retained_snapshot_detects_external_content_changes() {
        let root = crate::tests::temp_dir("batch-stale");
        let path = root.join("file");
        std::fs::write(&path, "original").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"original","new_text":"new"}]}]});
        let prepared = prepare(&request, None).unwrap();
        let snapshot = validate_snapshot(
            &request["files"][0],
            &prepared.targets[0],
            &mut (16 * 1024 * 1024),
            &|| false,
        )
        .unwrap();
        assert_eq!(snapshot.output, "new");
        snapshot.recheck(&prepared.targets[0], &|| false).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            let changed = std::fs::Permissions::from_mode(snapshot.permissions.mode() ^ 0o100);
            std::fs::set_permissions(&path, changed).unwrap();
            assert!(
                snapshot
                    .recheck(&prepared.targets[0], &|| false)
                    .unwrap_err()
                    .contains("permissions")
            );
            std::fs::set_permissions(&path, snapshot.permissions.clone()).unwrap();
            assert!(
                snapshot
                    .recheck(&prepared.targets[0], &|| false)
                    .unwrap_err()
                    .contains("metadata changed")
            );
        }
        std::fs::write(&path, "modified").unwrap();
        assert!(
            snapshot
                .recheck(&prepared.targets[0], &|| false)
                .unwrap_err()
                .contains("content changed")
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_validation_rejects_later_invalid_edit_without_writes() {
        let root = crate::tests::temp_dir("batch-snapshots");
        let first = root.join("first");
        let second = root.join("second");
        let original = "\u{feff}one\r\ntwo\nlast";
        std::fs::write(&first, original).unwrap();
        std::fs::write(&second, "other").unwrap();
        let request = json!({"files":[
            {"path":first,"edits":[{"old_text":"one","new_text":"ONE"}]},
            {"path":second,"edits":[{"old_text":"absent","new_text":"new"}]}
        ]});
        let descriptor = prepare(&request, None).unwrap();
        let polls = std::cell::Cell::new(0usize);
        let cancelled = || {
            polls.set(polls.get() + 1);
            polls.get() >= 4
        };
        assert!(
            verify_cancellable(&request, &descriptor, &cancelled)
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), original);
        assert!(
            verify(&request, &descriptor)
                .unwrap_err()
                .contains("file 2")
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "other");
        let mut valid = request;
        valid["files"][1]["edits"][0]["old_text"] = json!("other");
        verify(&valid, &prepare(&valid, None).unwrap()).unwrap();
        std::fs::write(&second, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
        assert!(
            verify(&valid, &prepare(&valid, None).unwrap())
                .unwrap_err()
                .contains("byte budget")
        );
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    #[cfg(not(unix))]
    fn multi_edit_refuses_unsupported_identity_checks_without_mutation() {
        let home = crate::tests::temp_dir("unsupported-batch");
        let path = home.join("source.txt");
        std::fs::write(&path, "original").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"original","new_text":"changed"}]}]});
        assert!(prepare(&request, None).unwrap_err().contains("unsupported"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn descriptor_rejects_unknown_versions_and_fields() {
        let target = json!({"path":std::env::temp_dir().join("target"),"device":1,"inode":2,"parent_device":1,"parent_inode":3});
        for count in [0, MAX_FILES + 1] {
            assert!(
                serde_json::from_value::<PreparedBatch>(json!({
                    "version":1,"request_digest":vec![0u8;32],"targets":vec![target.clone(); count]
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<PreparedBatch>(json!({
                "version":1,"request_digest":vec![0u8;32],"targets":(0..MAX_FILES).map(|index| json!({
                    "path":std::env::temp_dir().join(format!("target-{index}")),"device":1,"inode":index,
                    "parent_device":1,"parent_inode":3
                })).collect::<Vec<_>>()
            }))
            .is_ok()
        );
        for path in [
            "relative".to_owned(),
            "/../target".to_owned(),
            "/".to_owned(),
            format!("/{}", "x".repeat(MAX_PATH_BYTES)),
            "/nul\0".to_owned(),
        ] {
            let mut invalid = target.clone();
            invalid["path"] = json!(path);
            assert!(
                serde_json::from_value::<PreparedBatch>(
                    json!({"version":1,"request_digest":vec![0u8;32],"targets":[invalid]})
                )
                .is_err()
            );
        }
        let mut alias = target.clone();
        alias["path"] = json!("/other");
        assert!(
            serde_json::from_value::<PreparedBatch>(
                json!({"version":1,"request_digest":vec![0u8;32],"targets":[target,alias]})
            )
            .is_err()
        );
        for version in [0, 2, u32::MAX] {
            assert!(
                serde_json::from_value::<PreparedBatch>(json!({
                    "version":version,"targets":[target.clone()]
                }))
                .unwrap_err()
                .to_string()
                .contains("unsupported multi-edit preparation version")
            );
        }
        assert!(
            serde_json::from_value::<PreparedBatch>(json!({
                "version":1,"targets":[],"unexpected":true
            }))
            .is_err()
        );
    }

    // macOS filesystems reject invalid UTF-8 filenames at creation time.
    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_non_utf8_canonical_policy_path() {
        use std::os::unix::ffi::OsStringExt as _;
        let root = crate::tests::temp_dir("batch-non-utf8");
        let target = root.join(std::ffi::OsString::from_vec(vec![b'f', 0xff]));
        std::fs::write(&target, "original").unwrap();
        let alias = root.join("utf8-alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let request =
            json!({"files":[{"path":alias,"edits":[{"old_text":"original","new_text":"new"}]}]});
        assert!(
            prepare(&request, None)
                .unwrap_err()
                .contains("cannot be represented in policy facts")
        );
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_retains_identity_and_distinguishes_replacement() {
        let root = crate::tests::temp_dir("batch-identity");
        let path = root.join("target");
        std::fs::write(&path, "same contents").unwrap();
        let request =
            json!({"files":[{"path":path,"edits":[{"old_text":"same","new_text":"new"}]}]});
        let first = prepare(&request, None).unwrap();
        let encoded = serde_json::to_value(&first).unwrap();
        let decoded: PreparedBatch = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.targets[0].inode, first.targets[0].inode);
        verify(&request, &decoded).unwrap();
        let mut changed = request.clone();
        changed["files"][0]["edits"][0]["new_text"] = json!("different");
        assert!(
            verify(&changed, &decoded)
                .unwrap_err()
                .contains("does not match")
        );
        changed = request.clone();
        changed["files"][0]["path"] = json!(root.join("other"));
        assert!(
            verify(&changed, &decoded)
                .unwrap_err()
                .contains("does not match")
        );
        let replacement = root.join("replacement");
        std::fs::write(&replacement, "same contents").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(
            verify(&request, &decoded)
                .unwrap_err()
                .contains("identity changed")
        );
        let second = prepare(&request, None).unwrap();
        assert_ne!(
            (first.targets[0].device, first.targets[0].inode),
            (second.targets[0].device, second.targets[0].inode)
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_targets_reject_aliases_hard_links_and_directories() {
        let root = super::super::tests::temp_dir("batch-targets");
        let file = root.join("target");
        std::fs::write(&file, "original").unwrap();
        let entry =
            |path: &Path| json!({"path":path,"edits":[{"old_text":"original","new_text":"new"}]});
        let request = json!({"files":[entry(Path::new("target"))]});
        assert_eq!(
            prepare_paths(&request, Some(&root)).unwrap(),
            vec![file.canonicalize().unwrap()]
        );
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        assert!(
            prepare_paths(&json!({"files":[entry(&file),entry(&alias)]}), None)
                .unwrap_err()
                .contains("file 2: duplicate canonical target")
        );
        assert!(
            prepare_paths(&json!({"files":[entry(&root)]}), None)
                .unwrap_err()
                .contains("regular file")
        );
        let hard_link = root.join("hard-link");
        std::fs::hard_link(&file, &hard_link).unwrap();
        assert!(
            prepare_paths(&request, Some(&root))
                .unwrap_err()
                .contains("hard-linked")
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        std::fs::remove_file(hard_link).unwrap();
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn validates_all_files_before_access() {
        let request = json!({"files":[
            {"path":"/missing/target", "edits":[{"old_text":"a","new_text":"b"}]},
            {"path":"second", "edits":[{"old_text":"","new_text":"b"}]}
        ]});
        assert!(
            prepare_paths(&request, None)
                .unwrap_err()
                .contains("file 2, edit 1")
        );
    }

    #[test]
    fn rejects_shapes_and_enforces_aggregate_budgets() {
        for request in [
            json!({"files":[]}),
            json!({"files":[{"path":"a","edits":[]}]}),
            json!({"files":[{"path":"a","edits":[{"old_text":"x","new_text":1}]}]}),
        ] {
            assert!(validate(&request).is_err());
        }
        let file = json!({"path":"a","edits":[{"old_text":"x","new_text":""}]});
        assert!(validate(&json!({"files":vec![file; MAX_FILES + 1]})).is_err());
        let edit = json!({"old_text":"x","new_text":""});
        assert!(
            validate(&json!({"files":[{"path":"a","edits":vec![edit; MAX_EDITS + 1]}]})).is_err()
        );
        assert!(
            validate(&json!({"files":[{"path":"a","edits":[
            {"old_text":"x","new_text":"z".repeat(MAX_TEXT_BYTES)}]}]}))
            .is_err()
        );
    }
}
