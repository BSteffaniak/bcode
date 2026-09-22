//! Session-owned interpretation of local artifact references.

use std::path::{Path, PathBuf};

/// Resolve a local artifact reference without accessing the filesystem.
///
/// Supports invocation capabilities, relative paths, and historical absolute/file paths.
/// Callers must confine the resolved path beneath the owning artifact root before opening it;
/// successful parsing alone grants no filesystem authority.
///
/// # Errors
///
/// Rejects unsupported schemes, malformed capabilities, empty references and relative traversal.
pub fn resolve_artifact_reference(
    uri: &str,
    artifact_root: &Path,
) -> Result<PathBuf, &'static str> {
    if uri.is_empty() {
        return Err("artifact storage URI is empty");
    }
    if Path::new(uri).is_absolute() {
        return Ok(PathBuf::from(uri));
    }
    if let Ok(url) = url::Url::parse(uri) {
        if url.scheme() == "bcode-artifact" {
            if url.host_str() != Some("invocation")
                || !url.username().is_empty()
                || url.password().is_some()
                || url.port().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("artifact capability URI has an unsupported owner or fields");
            }
            let segments = url
                .path_segments()
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default();
            let [invocation, artifact] = segments.as_slice() else {
                return Err("artifact capability URI has an invalid path");
            };
            if ![invocation, artifact]
                .into_iter()
                .all(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return Err("artifact capability URI has an invalid identity");
            }
            return Ok(artifact_root
                .join("invocation-artifacts")
                .join(invocation)
                .join(format!("{artifact}.bin")));
        }
        if url.scheme() != "file" {
            return Err("artifact storage URI is not locally readable");
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err("artifact file URI has unsupported fields");
        }
        return url
            .to_file_path()
            .map_err(|()| "artifact file path is invalid");
    }
    let path = PathBuf::from(uri);
    if !path.is_absolute()
        && path.components().any(|part| {
            !matches!(
                part,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err("artifact relative storage path is invalid");
    }
    Ok(if path.is_absolute() {
        path
    } else {
        artifact_root.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_capabilities_and_rejects_ambiguous_fields() {
        let root = Path::new("/artifacts");
        let invocation = "a".repeat(64);
        let artifact = "b".repeat(64);
        let uri = format!("bcode-artifact://invocation/{invocation}/{artifact}");
        assert_eq!(
            resolve_artifact_reference(&uri, root).expect("capability"),
            root.join("invocation-artifacts")
                .join(&invocation)
                .join(format!("{artifact}.bin"))
        );
        for bad in [
            format!("{uri}?x=1"),
            format!("{uri}#x"),
            format!("{uri}/extra"),
            uri.replace("invocation/", "other/"),
            uri.replace("bcode-artifact://", "bcode-artifact://user@"),
            "bcode-artifact://invocation/short/short".to_owned(),
        ] {
            assert!(resolve_artifact_reference(&bad, root).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_legacy_local_paths_but_never_remote_or_traversing_references() {
        let root = Path::new("/artifacts");
        assert_eq!(
            resolve_artifact_reference("recordings/run.bin", root).expect("relative"),
            root.join("recordings/run.bin")
        );
        let legacy = std::env::temp_dir().join("old.bin");
        let uri = url::Url::from_file_path(&legacy).expect("file URI");
        assert_eq!(
            resolve_artifact_reference(uri.as_str(), root).expect("file"),
            legacy
        );
        assert_eq!(
            resolve_artifact_reference(legacy.to_str().expect("path"), root).expect("absolute"),
            legacy
        );
        for bad in [
            "",
            "../escape",
            "nested/../../escape",
            "https://example.com/data",
            "file:///tmp/a?query=1",
        ] {
            assert!(resolve_artifact_reference(bad, root).is_err(), "{bad}");
        }
    }
}
