//! Generic invocation-bound workspace discovery authorization.
//!
//! The host bridge is bound to the active outer invocation. Nested calls inherit
//! identity, cancellation, deadline and workspace context; callers cannot provide
//! invocation identities or grants in workspace discovery payloads. Unsupported or
//! denied authorization fails closed. Discovery grants never authorize contents;
//! separate resource grants authorize bounded reads of one requested resource only.
//! Neither grant authorizes execution, writes or model attachments.

use serde::{Deserialize, Serialize};
use std::path::{Component, PathBuf};

/// Invocation-bound host capability interface; unknown versions are unsupported.
pub const WORKSPACE_ACCESS_INTERFACE_ID: &str = "bcode.workspace-access/v1";
/// Operation accepting `WorkspaceAccessRequest` and returning `WorkspaceAccessGrant`.
pub const OP_WORKSPACE_ACCESS_AUTHORIZE: &str = "authorize";
/// Operation accepting `WorkspaceResourceRequest` and returning `WorkspaceResourceGrant`.
pub const OP_WORKSPACE_RESOURCE_AUTHORIZE: &str = "authorize_resource";
/// Authorize one absolute resource outside the workspace without changing workspace authority.
pub const OP_EXTERNAL_RESOURCE_AUTHORIZE: &str = "authorize_external_resource";

/// Request a bounded exact-resource read under the bound invocation's read policy.
///
/// This grants no discovery, workspace selection, write, attachment or execution authority.
/// Absolute paths must be normalized and canonical without symlink aliases; the host
/// returns the authorized parent root and basename only after evaluating read policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalResourceRequest {
    /// Exact absolute resource path; traversal and ambiguous components are rejected.
    pub path: PathBuf,
}

impl ExternalResourceRequest {
    /// Whether this names one unambiguous absolute resource, not a root or traversal.
    #[must_use]
    pub fn is_normalized(&self) -> bool {
        self.path.is_absolute()
            && self.path.file_name().is_some()
            && self.path.components().all(|component| {
                matches!(
                    component,
                    Component::RootDir | Component::Prefix(_) | Component::Normal(_)
                )
            })
    }
}

/// Request permission for a bounded read of one workspace-relative resource.
///
/// The host resolves canonical confinement and evaluates `filesystem.read` Read
/// policy plus active skill restrictions under the existing bound invocation.
/// This request supplies no root, invocation identity or authorization decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceResourceRequest {
    /// Normalized, nonempty workspace-relative path of the requested resource.
    pub path: String,
}

/// Host-produced permission to read exactly one requested resource.
///
/// Consumers match the returned path to the request and enforce descriptor-based
/// confinement and bounded reads. Hosts reject symlink aliases and validate a
/// confined regular file before applying read policy; consumers do not read any
/// contents before receiving the grant. The grant inherits invocation cancellation and
/// deadline; it cannot authorize any other resource, attachment, write or execution.
/// Unsupported operations and malformed/mismatched grants fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceResourceGrant {
    /// Absolute canonical workspace root resolved and authorized by the host.
    pub workspace_root: PathBuf,
    /// Exact normalized relative path authorized for a bounded read.
    pub path: String,
}

impl WorkspaceResourceRequest {
    /// Validate nonempty lexical normalization, not authorization or confinement.
    #[must_use]
    pub fn is_normalized(&self) -> bool {
        !self.path.is_empty() && normalized_relative(&self.path)
    }
}

/// Portable outer service context hints; these are not grants or policy decisions.
///
/// Transport validates session identity and derives its canonical working directory.
/// Without a session, the host canonicalizes `launch_root` as an existing directory
/// and evaluates discovery policy before returning any grant. Missing context does
/// not authorize falling back to a daemon or plugin process working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContext {
    /// Optional session identity, checked by the application boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional frontend launch-directory hint, never trusted as canonical authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_root: Option<PathBuf>,
}

/// Canonical discovery facts requested from an invocation-bound host capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAccessRequest {
    /// Normalized workspace-relative directory; empty means workspace root.
    /// Absolute paths, parent/current components and empty interior components are invalid.
    pub directory: String,
    /// Whether discovery extends recursively beneath the requested directory.
    pub recursive: bool,
    /// Whether hidden entries are requested; policy must authorize this choice.
    pub include_hidden: bool,
}

/// Host-produced discovery grant, never decoded from caller-controlled search JSON.
///
/// Host authorization evaluates canonical list/discovery policy facts. Denied
/// subroots may be represented as excluded prefixes; if policy cannot be safely
/// represented this way, the host must deny rather than broaden confinement.
/// Consumers must independently enforce canonical confinement during traversal,
/// including symlink handling. Grant possession does not authorize reading contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAccessGrant {
    /// Absolute canonical workspace directory resolved and authorized by the host.
    pub workspace_root: PathBuf,
    /// Literal normalized relative excluded subroots. Empty prefix denies all.
    /// Invalid prefixes fail closed; this is not a glob or SQL expression language.
    pub excluded_prefixes: Vec<PathBuf>,
}

impl WorkspaceAccessRequest {
    /// Validate lexical normalization before host canonicalization and policy checks.
    /// This check is not itself authorization or proof of canonical confinement.
    #[must_use]
    pub fn is_normalized(&self) -> bool {
        normalized_relative(&self.directory)
    }
}

fn normalized_relative(directory: &str) -> bool {
    directory.is_empty()
        || (directory.len() <= 4096
            && !directory.chars().any(char::is_control)
            && !directory.contains('\\')
            && directory
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_relative_discovery_scope() {
        for directory in ["", "src", "src/文件", "a b/c"] {
            assert!(
                WorkspaceAccessRequest {
                    directory: directory.into(),
                    recursive: true,
                    include_hidden: false
                }
                .is_normalized()
            );
        }
        for directory in [
            "/tmp", "../src", "./src", "src/", "src//x", "src/../x", "a\\b", "a\0b",
        ] {
            assert!(
                !WorkspaceAccessRequest {
                    directory: directory.into(),
                    recursive: true,
                    include_hidden: false
                }
                .is_normalized()
            );
        }
    }

    #[test]
    fn resource_scope_is_exact_nonempty_and_typed() {
        for path in ["config.txt", "src/文件", "a b/c"] {
            let request = WorkspaceResourceRequest { path: path.into() };
            assert!(request.is_normalized());
            assert_eq!(
                serde_json::from_slice::<WorkspaceResourceRequest>(
                    &serde_json::to_vec(&request).unwrap()
                )
                .unwrap(),
                request
            );
        }
        for path in [
            "",
            "/etc/config",
            "../config",
            "./config",
            "a/",
            "a//b",
            "a/../b",
            "a\\b",
            "a\0b",
        ] {
            assert!(!WorkspaceResourceRequest { path: path.into() }.is_normalized());
        }
        assert!(
            !WorkspaceResourceRequest {
                path: "x".repeat(4097)
            }
            .is_normalized()
        );
        assert!(
            serde_json::from_str::<WorkspaceResourceRequest>(
                r#"{"path":"config.txt","workspace_root":"/untrusted"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WorkspaceResourceRequest>(
                r#"{"path":"config.txt","invocation_id":"spoof"}"#
            )
            .is_err()
        );
        let grant = WorkspaceResourceGrant {
            workspace_root: PathBuf::from("/workspace"),
            path: "config.txt".into(),
        };
        assert_eq!(
            serde_json::from_slice::<WorkspaceResourceGrant>(&serde_json::to_vec(&grant).unwrap())
                .unwrap(),
            grant
        );
        assert!(
            serde_json::from_str::<WorkspaceResourceGrant>(r#"{"workspace_root":"/workspace"}"#)
                .is_err()
        );
    }

    #[test]
    fn context_and_capability_serde_are_typed_not_grants_from_requests() {
        let context: WorkspaceContext = serde_json::from_str("{}").unwrap();
        assert_eq!(context, WorkspaceContext::default());
        let context = WorkspaceContext {
            session_id: Some("session".into()),
            launch_root: Some(PathBuf::from("/workspace")),
        };
        assert_eq!(
            serde_json::from_slice::<WorkspaceContext>(&serde_json::to_vec(&context).unwrap())
                .unwrap(),
            context
        );
        assert!(serde_json::from_str::<WorkspaceAccessRequest>(r#"{"directory":"","recursive":true,"include_hidden":false,"workspace_root":"/untrusted"}"#).is_err());
        assert!(serde_json::from_str::<WorkspaceAccessRequest>(r#"{"directory":"","recursive":true,"include_hidden":false,"invocation_id":"spoof"}"#).is_err());
    }
}
