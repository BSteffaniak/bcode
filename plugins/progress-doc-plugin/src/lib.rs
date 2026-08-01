#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Confined, bounded progress-document workflow blocks.
//!
//! Full progress-document content is confined to typed block input/output. Routine diagnostics and
//! plugin errors expose only bounded metadata and normalized messages, never document content.

use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

const CONTRACT_VERSION: u32 = 1;
const MAX_DOCUMENT_BYTES: u64 = 512 * 1024;
const MAX_TASK_ITEMS: usize = 4_096;
const MAX_UNRESOLVED_SUMMARIES: usize = 128;
const MAX_UNRESOLVED_SUMMARY_BYTES: usize = 512;
const MAX_APPROVAL_PROVENANCE_BYTES: usize = 4_096;

/// Progress-document workflow plugin.
#[derive(Default)]
pub struct ProgressDocPlugin;

impl RustPlugin for ProgressDocPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported progress-document service interface",
            );
        }
        invoke_workflow_block(&context)
    }
}

#[derive(Debug, Error)]
enum ProgressDocError {
    #[error("invalid progress-document request: {0}")]
    InvalidRequest(String),
    #[error("progress-document path is outside the workflow workspace")]
    OutsideWorkspace,
    #[error("progress-document path is ambiguous or contains a symlink")]
    AmbiguousPath,
    #[error("progress document exceeds {MAX_DOCUMENT_BYTES} bytes")]
    Oversized,
    #[error("progress document is not valid UTF-8")]
    InvalidEncoding,
    #[error("progress document state conflicts with the approved mutation")]
    Conflict,
    #[error("progress-document I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl ProgressDocError {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::OutsideWorkspace | Self::AmbiguousPath => "path_not_confined",
            Self::Oversized => "document_too_large",
            Self::InvalidEncoding => "invalid_encoding",
            Self::Conflict => "state_conflict",
            Self::Io(_) => "io_failed",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectRequest {
    version: u32,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Inspection {
    version: u32,
    path: String,
    exists: bool,
    content_sha256: Option<String>,
    byte_length: u64,
    checked_task_count: u32,
    unchecked_task_count: u32,
    total_task_count: u32,
    parse_complete: bool,
    unresolved_summaries: Vec<String>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationRequest {
    version: u32,
    path: PathBuf,
    #[serde(default)]
    expected_absent: bool,
    #[serde(default)]
    expected_sha256: Option<String>,
    desired_content: String,
    desired_sha256: String,
    approval_provenance: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MutationOperation {
    Created,
    Replaced,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct MutationResult {
    version: u32,
    operation: MutationOperation,
    path: String,
    previous_sha256: Option<String>,
    content_sha256: String,
    byte_length: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconcileRequest {
    version: u32,
    path: PathBuf,
    expected_previous_sha256: Option<String>,
    desired_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReconciliationOutcome {
    NotApplied,
    Applied,
    Diverged,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Reconciliation {
    version: u32,
    path: String,
    outcome: ReconciliationOutcome,
    actual_sha256: Option<String>,
    byte_length: u64,
}

fn invoke_workflow_block(context: &NativeServiceContext) -> ServiceResponse {
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "progress-document operation cancelled");
    }
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let result = match context.request.operation.as_str() {
        "progress-doc.inspect" => invocation
            .typed_input::<InspectRequest>()
            .map_err(ProgressDocError::InvalidRequest)
            .and_then(|request| inspect(&invocation.workspace_root, &request))
            .and_then(|value| encode_response(&value)),
        "progress-doc.create" => invocation
            .typed_input::<MutationRequest>()
            .map_err(ProgressDocError::InvalidRequest)
            .and_then(|request| {
                mutate(
                    &invocation.workspace_root,
                    &request,
                    MutationOperation::Created,
                )
            })
            .and_then(|value| encode_response(&value)),
        "progress-doc.replace" => invocation
            .typed_input::<MutationRequest>()
            .map_err(ProgressDocError::InvalidRequest)
            .and_then(|request| {
                mutate(
                    &invocation.workspace_root,
                    &request,
                    MutationOperation::Replaced,
                )
            })
            .and_then(|value| encode_response(&value)),
        "progress-doc.reconcile" => invocation
            .typed_input::<ReconcileRequest>()
            .map_err(ProgressDocError::InvalidRequest)
            .and_then(|request| reconcile(&invocation.workspace_root, &request))
            .and_then(|value| encode_response(&value)),
        _ => {
            return ServiceResponse::error(
                "unsupported_operation",
                "unsupported progress-document workflow block operation",
            );
        }
    };
    result.unwrap_or_else(|error| ServiceResponse::error(error.code(), error.to_string()))
}

fn encode_response<T: Serialize>(value: &T) -> Result<ServiceResponse, ProgressDocError> {
    ServiceResponse::json(value)
        .map_err(|error| ProgressDocError::InvalidRequest(error.to_string()))
}

fn inspect(root: &Path, request: &InspectRequest) -> Result<Inspection, ProgressDocError> {
    validate_version(request.version)?;
    let confined = ConfinedPath::new(root, &request.path)?;
    let Some(document) = read_document(&confined)? else {
        return Ok(Inspection {
            version: CONTRACT_VERSION,
            path: confined.relative,
            exists: false,
            content_sha256: None,
            byte_length: 0,
            checked_task_count: 0,
            unchecked_task_count: 0,
            total_task_count: 0,
            parse_complete: true,
            unresolved_summaries: Vec::new(),
            truncated: false,
        });
    };
    Ok(parse_inspection(confined.relative, &document))
}

fn mutate(
    root: &Path,
    request: &MutationRequest,
    operation: MutationOperation,
) -> Result<MutationResult, ProgressDocError> {
    validate_version(request.version)?;
    validate_digest(&request.desired_sha256)?;
    if request.approval_provenance.trim().is_empty()
        || request.approval_provenance.len() > MAX_APPROVAL_PROVENANCE_BYTES
        || request.desired_content.len() > usize::try_from(MAX_DOCUMENT_BYTES).unwrap_or(usize::MAX)
        || sha256_hex(request.desired_content.as_bytes()) != request.desired_sha256
    {
        return Err(ProgressDocError::InvalidRequest(
            "desired content, digest, or approval provenance is invalid".to_string(),
        ));
    }
    let confined = ConfinedPath::new(root, &request.path)?;
    let previous = read_document(&confined)?;
    let previous_sha256 = previous.as_ref().map(|bytes| sha256_hex(bytes));
    match operation {
        MutationOperation::Created => {
            if !request.expected_absent || request.expected_sha256.is_some() || previous.is_some() {
                return Err(ProgressDocError::Conflict);
            }
        }
        MutationOperation::Replaced => {
            let expected = request.expected_sha256.as_deref().ok_or_else(|| {
                ProgressDocError::InvalidRequest(
                    "replace requires an expected current SHA-256".to_string(),
                )
            })?;
            validate_digest(expected)?;
            if request.expected_absent || previous_sha256.as_deref() != Some(expected) {
                return Err(ProgressDocError::Conflict);
            }
        }
    }
    match operation {
        MutationOperation::Created => {
            create_document(&confined, request.desired_content.as_bytes())?;
        }
        MutationOperation::Replaced => replace_document(
            &confined,
            request
                .expected_sha256
                .as_deref()
                .expect("validated replace digest"),
            request.desired_content.as_bytes(),
        )?,
    }
    Ok(MutationResult {
        version: CONTRACT_VERSION,
        operation,
        path: confined.relative,
        previous_sha256,
        content_sha256: request.desired_sha256.clone(),
        byte_length: u64::try_from(request.desired_content.len()).unwrap_or(u64::MAX),
    })
}

fn reconcile(root: &Path, request: &ReconcileRequest) -> Result<Reconciliation, ProgressDocError> {
    validate_version(request.version)?;
    validate_digest(&request.desired_sha256)?;
    if let Some(previous) = &request.expected_previous_sha256 {
        validate_digest(previous)?;
    }
    let confined = ConfinedPath::new(root, &request.path)?;
    let current = read_document(&confined)?;
    let actual_sha256 = current.as_ref().map(|bytes| sha256_hex(bytes));
    let outcome = if actual_sha256.as_deref() == Some(request.desired_sha256.as_str()) {
        ReconciliationOutcome::Applied
    } else if actual_sha256 == request.expected_previous_sha256 {
        ReconciliationOutcome::NotApplied
    } else {
        ReconciliationOutcome::Diverged
    };
    Ok(Reconciliation {
        version: CONTRACT_VERSION,
        path: confined.relative,
        outcome,
        actual_sha256,
        byte_length: current
            .as_ref()
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
    })
}

struct ConfinedPath {
    absolute: PathBuf,
    relative: String,
}

impl ConfinedPath {
    fn new(root: &Path, relative: &Path) -> Result<Self, ProgressDocError> {
        if !root.is_absolute() || relative.as_os_str().is_empty() || relative.is_absolute() {
            return Err(ProgressDocError::OutsideWorkspace);
        }
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        }) {
            return Err(ProgressDocError::OutsideWorkspace);
        }
        let root = root.canonicalize()?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let canonical_parent = root.join(parent).canonicalize()?;
        if !canonical_parent.starts_with(&root) {
            return Err(ProgressDocError::OutsideWorkspace);
        }
        let name = relative
            .file_name()
            .ok_or(ProgressDocError::AmbiguousPath)?;
        let absolute = canonical_parent.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&absolute) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ProgressDocError::AmbiguousPath);
            }
            let canonical = absolute.canonicalize()?;
            if !canonical.starts_with(&root) || canonical != absolute {
                return Err(ProgressDocError::AmbiguousPath);
            }
        }
        Ok(Self {
            absolute,
            relative: relative.to_string_lossy().replace('\\', "/"),
        })
    }
}

fn read_document(path: &ConfinedPath) -> Result<Option<Vec<u8>>, ProgressDocError> {
    let mut file = match OpenOptions::new().read(true).open(&path.absolute) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ProgressDocError::AmbiguousPath);
    }
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(ProgressDocError::Oversized);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_DOCUMENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > usize::try_from(MAX_DOCUMENT_BYTES).unwrap_or(usize::MAX) {
        return Err(ProgressDocError::Oversized);
    }
    std::str::from_utf8(&bytes).map_err(|_| ProgressDocError::InvalidEncoding)?;
    Ok(Some(bytes))
}

fn create_document(path: &ConfinedPath, bytes: &[u8]) -> Result<(), ProgressDocError> {
    let parent = path
        .absolute
        .parent()
        .ok_or(ProgressDocError::AmbiguousPath)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&path.absolute)
        .map_err(|error| match error.error.kind() {
            std::io::ErrorKind::AlreadyExists => ProgressDocError::Conflict,
            _ => ProgressDocError::Io(error.error),
        })?;
    sync_parent(parent)?;
    Ok(())
}

fn replace_document(
    path: &ConfinedPath,
    expected_sha256: &str,
    bytes: &[u8],
) -> Result<(), ProgressDocError> {
    let parent = path
        .absolute
        .parent()
        .ok_or(ProgressDocError::AmbiguousPath)?;
    let metadata = fs::symlink_metadata(&path.absolute).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProgressDocError::Conflict
        } else {
            ProgressDocError::Io(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProgressDocError::AmbiguousPath);
    }
    let lock_path = replacement_lock_path(&path.absolute)?;
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => ProgressDocError::Conflict,
            _ => ProgressDocError::Io(error),
        })?;
    let result = (|| {
        let current = read_document(path)?.ok_or(ProgressDocError::Conflict)?;
        if sha256_hex(&current) != expected_sha256 {
            return Err(ProgressDocError::Conflict);
        }
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&path.absolute)
            .map_err(|error| ProgressDocError::Io(error.error))?;
        sync_parent(parent)
    })();
    drop(lock);
    let remove_result = fs::remove_file(&lock_path);
    match (result, remove_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(ProgressDocError::Io(error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn replacement_lock_path(path: &Path) -> Result<PathBuf, ProgressDocError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ProgressDocError::AmbiguousPath)?;
    Ok(path.with_file_name(format!(".{file_name}.bcode-progress.lock")))
}

fn sync_parent(parent: &Path) -> Result<(), ProgressDocError> {
    OpenOptions::new().read(true).open(parent)?.sync_all()?;
    Ok(())
}

fn parse_inspection(path: String, bytes: &[u8]) -> Inspection {
    let content = std::str::from_utf8(bytes).expect("document encoding was validated");
    let mut checked = 0_u32;
    let mut unchecked = 0_u32;
    let mut total = 0_usize;
    let mut unresolved_summaries = Vec::new();
    let mut parse_complete = true;
    let mut summaries_truncated = false;
    for line in content.lines() {
        let Some((is_checked, summary)) = task_item(line) else {
            continue;
        };
        if total == MAX_TASK_ITEMS {
            parse_complete = false;
            continue;
        }
        total += 1;
        if is_checked {
            checked = checked.saturating_add(1);
        } else {
            unchecked = unchecked.saturating_add(1);
            if unresolved_summaries.len() < MAX_UNRESOLVED_SUMMARIES {
                unresolved_summaries.push(truncate_utf8(summary, MAX_UNRESOLVED_SUMMARY_BYTES));
            } else {
                summaries_truncated = true;
            }
        }
    }
    Inspection {
        version: CONTRACT_VERSION,
        path,
        exists: true,
        content_sha256: Some(sha256_hex(bytes)),
        byte_length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        checked_task_count: checked,
        unchecked_task_count: unchecked,
        total_task_count: u32::try_from(total).unwrap_or(u32::MAX),
        parse_complete,
        unresolved_summaries,
        truncated: !parse_complete || summaries_truncated,
    }
}

fn task_item(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    let after_marker = if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        rest
    } else {
        let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count == 0 {
            return None;
        }
        let rest = &trimmed[digit_count..];
        rest.strip_prefix(". ")
            .or_else(|| rest.strip_prefix(") "))?
    };
    after_marker.strip_prefix("[ ] ").map_or_else(
        || {
            after_marker
                .strip_prefix("[x] ")
                .or_else(|| after_marker.strip_prefix("[X] "))
                .map_or_else(
                    || {
                        if after_marker == "[ ]" {
                            Some((false, ""))
                        } else if after_marker == "[x]" || after_marker == "[X]" {
                            Some((true, ""))
                        } else {
                            None
                        }
                    },
                    |summary| Some((true, summary.trim())),
                )
        },
        |summary| Some((false, summary.trim())),
    )
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn validate_version(version: u32) -> Result<(), ProgressDocError> {
    if version == CONTRACT_VERSION {
        Ok(())
    } else {
        Err(ProgressDocError::InvalidRequest(
            "unsupported progress-document contract version".to_string(),
        ))
    }
}

fn validate_digest(value: &str) -> Result<(), ProgressDocError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(ProgressDocError::InvalidRequest(
            "SHA-256 must be 64 lowercase hexadecimal characters".to_string(),
        ))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(ProgressDocPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(ProgressDocPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_parser_is_bounded_and_supports_markdown_markers() {
        let content = b"# Work\n- [x] done\n* [ ] first\n1. [X] also done\n2) [ ] second\n";
        let inspection = parse_inspection("local-progress.md".to_string(), content);
        assert_eq!(inspection.checked_task_count, 2);
        assert_eq!(inspection.unchecked_task_count, 2);
        assert_eq!(inspection.total_task_count, 4);
        assert_eq!(inspection.unresolved_summaries, ["first", "second"]);
        assert!(inspection.parse_complete);
        assert!(!inspection.truncated);
    }

    #[test]
    fn create_replace_inspect_and_reconcile_use_exact_digests() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = PathBuf::from("local-work-progress.md");
        let first = "- [ ] first\n";
        let first_digest = sha256_hex(first.as_bytes());
        let created = mutate(
            workspace.path(),
            &MutationRequest {
                version: CONTRACT_VERSION,
                path: path.clone(),
                expected_absent: true,
                expected_sha256: None,
                desired_content: first.to_string(),
                desired_sha256: first_digest.clone(),
                approval_provenance: "interaction:create".to_string(),
            },
            MutationOperation::Created,
        )
        .expect("create");
        assert_eq!(created.operation, MutationOperation::Created);
        let inspection = inspect(
            workspace.path(),
            &InspectRequest {
                version: CONTRACT_VERSION,
                path: path.clone(),
            },
        )
        .expect("inspect");
        assert_eq!(inspection.unchecked_task_count, 1);
        assert_eq!(
            inspection.content_sha256.as_deref(),
            Some(first_digest.as_str())
        );

        let second = "- [x] first\n";
        let second_digest = sha256_hex(second.as_bytes());
        mutate(
            workspace.path(),
            &MutationRequest {
                version: CONTRACT_VERSION,
                path: path.clone(),
                expected_absent: false,
                expected_sha256: Some(first_digest.clone()),
                desired_content: second.to_string(),
                desired_sha256: second_digest.clone(),
                approval_provenance: "interaction:replace".to_string(),
            },
            MutationOperation::Replaced,
        )
        .expect("replace");
        let reconciliation = reconcile(
            workspace.path(),
            &ReconcileRequest {
                version: CONTRACT_VERSION,
                path,
                expected_previous_sha256: Some(first_digest),
                desired_sha256: second_digest,
            },
        )
        .expect("reconcile");
        assert_eq!(reconciliation.outcome, ReconciliationOutcome::Applied);
    }

    #[test]
    fn mutation_rejects_stale_state_and_unconfined_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let request = MutationRequest {
            version: CONTRACT_VERSION,
            path: PathBuf::from("../outside.md"),
            expected_absent: true,
            expected_sha256: None,
            desired_content: "content".to_string(),
            desired_sha256: sha256_hex(b"content"),
            approval_provenance: "interaction".to_string(),
        };
        assert!(matches!(
            mutate(workspace.path(), &request, MutationOperation::Created),
            Err(ProgressDocError::OutsideWorkspace)
        ));

        fs::write(workspace.path().join("existing.md"), "existing").expect("existing");
        let stale = MutationRequest {
            path: PathBuf::from("existing.md"),
            expected_absent: false,
            expected_sha256: Some(sha256_hex(b"stale")),
            ..request
        };
        assert!(matches!(
            mutate(workspace.path(), &stale, MutationOperation::Replaced),
            Err(ProgressDocError::Conflict)
        ));
    }

    #[test]
    fn malformed_encoding_is_rejected_without_content_disclosure() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("invalid.md"), [0xff, 0xfe]).expect("invalid document");
        let error = inspect(
            workspace.path(),
            &InspectRequest {
                version: CONTRACT_VERSION,
                path: PathBuf::from("invalid.md"),
            },
        )
        .expect_err("encoding must fail closed");
        assert!(matches!(error, ProgressDocError::InvalidEncoding));
        assert!(!error.to_string().contains("ff"));
    }

    fn workflow_context(
        workspace_root: &Path,
        cancellation: bcode_plugin_sdk::ServiceCancellation,
    ) -> NativeServiceContext {
        let invocation = bcode_workflow::WorkflowBlockInvocation {
            version: bcode_workflow::WorkflowBlockInvocation::VERSION,
            dispatch_identity: "progress-doc-test".to_string(),
            workspace_root: workspace_root.to_path_buf(),
            input: serde_json::json!({
                "version": CONTRACT_VERSION,
                "path": "cancelled.md",
                "expected_absent": true,
                "desired_content": "must not be written",
                "desired_sha256": sha256_hex(b"must not be written"),
                "approval_provenance": "interaction:cancelled"
            }),
        };
        NativeServiceContext {
            plugin_id: "bcode.progress-doc".to_string(),
            request: ServiceRequest {
                interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
                operation: "progress-doc.create".to_string(),
                payload: serde_json::to_vec(&invocation).expect("invocation"),
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation,
            bridge: ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        }
    }

    #[test]
    fn cancellation_precedes_progress_document_mutation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        cancellation.cancel();
        let response = invoke_workflow_block(&workflow_context(workspace.path(), cancellation));
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("cancelled")
        );
        assert!(!workspace.path().join("cancelled.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_rejected() {
        use std::os::unix::fs::symlink;
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        symlink(outside.path(), workspace.path().join("linked.md")).expect("symlink");
        assert!(matches!(
            inspect(
                workspace.path(),
                &InspectRequest {
                    version: CONTRACT_VERSION,
                    path: PathBuf::from("linked.md"),
                }
            ),
            Err(ProgressDocError::AmbiguousPath)
        ));
    }

    #[test]
    fn manifest_declares_exact_authorization_and_reconciliation() {
        let manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let blocks = &manifest.services[0].workflow_blocks;
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].block_id, "progress-doc.inspect");
        assert_eq!(blocks[1].block_id, "progress-doc.create");
        assert_eq!(blocks[2].block_id, "progress-doc.replace");
        assert_eq!(blocks[3].block_id, "progress-doc.reconcile");
        for block in [&blocks[1], &blocks[2]] {
            assert_eq!(block.effect, bcode_workflow::WorkflowBlockEffect::Mutating);
            assert!(block.authorization.explicit_grant_required);
            assert_eq!(
                block.reconciliation,
                bcode_workflow::WorkflowBlockReconciliation::RepairRequired
            );
        }
    }
}
