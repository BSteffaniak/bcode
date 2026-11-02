//! Bounded discovery of provider-declared external static credentials.
//!
//! Discovery never grants import authority. Values remain request-local and are
//! re-read only for a source explicitly selected by the caller.

use bcode_provider_auth_models::{AuthCredentialSource, AuthSecretField};
use std::io::Read as _;
use std::path::{Component, Path};
use zeroize::Zeroizing;

const MAX_SOURCE_BYTES: u64 = 1024 * 1024;

/// Sanitized source availability, not proof of remote authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSourceStatus {
    /// A locally valid static credential is available.
    Available,
    /// Source or supported credential is absent.
    Missing,
    /// Access, format, confinement, or local validation failed.
    Unavailable,
}

/// Discover a field's declared sources without exposing credential values.
/// Disabled discovery returns before inspecting any source.
#[must_use]
pub fn discover(
    enabled: bool,
    home: &Path,
    field: &AuthSecretField,
) -> Vec<(usize, CredentialSourceStatus)> {
    if !enabled || field.validate().is_err() {
        return Vec::new();
    }
    field
        .discovery_sources
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let status = match read_selected(home, field, index) {
                Ok(Some(_)) => CredentialSourceStatus::Available,
                Ok(None) => CredentialSourceStatus::Missing,
                Err(error) => error,
            };
            (index, status)
        })
        .collect()
}

/// Read exactly one explicitly selected, provider-declared source.
///
/// # Errors
/// Returns a secret-free unavailable status for invalid declarations, inaccessible
/// or oversized files, escaping paths, malformed JSON, and invalid credentials.
pub fn read_selected(
    home: &Path,
    field: &AuthSecretField,
    index: usize,
) -> Result<Option<Zeroizing<String>>, CredentialSourceStatus> {
    field
        .validate()
        .map_err(|_| CredentialSourceStatus::Unavailable)?;
    let source = field
        .discovery_sources
        .get(index)
        .ok_or(CredentialSourceStatus::Unavailable)?;
    let value = match source {
        AuthCredentialSource::Environment { name } => match std::env::var(name) {
            Ok(value) => Some(Zeroizing::new(value)),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(CredentialSourceStatus::Unavailable);
            }
        },
        AuthCredentialSource::JsonFile {
            relative_path,
            pointer,
            discriminator,
            ..
        } => read_json_source(home, relative_path, pointer, discriminator.as_ref())?,
    };
    if let Some(value) = &value {
        if value.is_empty() {
            return Ok(None);
        }
        field
            .validation
            .validate_secret(value)
            .map_err(|_| CredentialSourceStatus::Unavailable)?;
    }
    Ok(value)
}

fn read_json_source(
    home: &Path,
    relative_path: &str,
    pointer: &str,
    discriminator: Option<&(String, String)>,
) -> Result<Option<Zeroizing<String>>, CredentialSourceStatus> {
    let unavailable = |_| CredentialSourceStatus::Unavailable;
    let file = open_confined(home, relative_path)?;
    let Some(file) = file else {
        return Ok(None);
    };
    let metadata = file.metadata().map_err(unavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return Err(CredentialSourceStatus::Unavailable);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(CredentialSourceStatus::Unavailable);
    }
    let mut document: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| CredentialSourceStatus::Unavailable)?;
    let matches = discriminator.is_none_or(|(key, expected)| {
        document.pointer(key).and_then(serde_json::Value::as_str) == Some(expected.as_str())
    });
    let value = if matches {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(|value| Zeroizing::new(value.to_owned()))
    } else {
        None
    };
    clear_json_strings(&mut document);
    Ok(value)
}

fn open_confined(
    home: &Path,
    relative: &str,
) -> Result<Option<std::fs::File>, CredentialSourceStatus> {
    let parts = Path::new(relative).components().collect::<Vec<_>>();
    let Some((last, ancestors)) = parts.split_last() else {
        return Err(CredentialSourceStatus::Unavailable);
    };
    if parts
        .iter()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(CredentialSourceStatus::Unavailable);
    }
    let missing_or_unavailable = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(CredentialSourceStatus::Unavailable)
        }
    };
    // Each component is opened relative to the held directory without following links,
    // so path replacement cannot escape the root.
    let mut directory = bcode_confined_fs::Dir::open_following_links(home)
        .map_err(|_| CredentialSourceStatus::Unavailable)?;
    for part in ancestors {
        directory = match directory.open_dir(part.as_os_str()) {
            Ok(directory) => directory,
            Err(error) => return missing_or_unavailable(error),
        };
    }
    match directory.open_file(
        last.as_os_str(),
        bcode_confined_fs::OpenOptions::new().read(true),
    ) {
        Ok(file) => Ok(Some(file)),
        Err(error) => missing_or_unavailable(error),
    }
}

fn clear_json_strings(value: &mut serde_json::Value) {
    use zeroize::Zeroize as _;
    match value {
        serde_json::Value::String(text) => text.zeroize(),
        serde_json::Value::Array(values) => values.iter_mut().for_each(clear_json_strings),
        serde_json::Value::Object(values) => values.values_mut().for_each(clear_json_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field() -> AuthSecretField {
        AuthSecretField {
            credential_id: "api_key".to_owned(),
            storage_key: "API_KEY".to_owned(),
            prompt: "API key".to_owned(),
            optional: false,
            validation: bcode_provider_auth_models::AuthSecretValidation::default(),
            invocation_env: Vec::new(),
            discovery_sources: vec![AuthCredentialSource::JsonFile {
                application: "Test".to_owned(),
                relative_path: "auth.json".to_owned(),
                pointer: "/provider/key".to_owned(),
                discriminator: Some(("/provider/type".to_owned(), "api".to_owned())),
            }],
        }
    }

    #[test]
    fn disabled_discovery_does_not_access_missing_root() {
        assert_eq!(
            discover(false, Path::new("/nonexistent/bcode-test"), &field()),
            [] as [(usize, crate::discovery::CredentialSourceStatus); 0]
        );
    }

    #[test]
    fn reads_only_compatible_static_keys_and_preserves_source() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("auth.json");
        let contents = r#"{"provider":{"type":"api","key":"test-value"}}"#;
        std::fs::write(&path, contents).unwrap();
        assert_eq!(
            discover(true, home.path(), &field()),
            vec![(0, CredentialSourceStatus::Available)]
        );
        assert_eq!(
            read_selected(home.path(), &field(), 0)
                .unwrap()
                .unwrap()
                .as_str(),
            "test-value"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        std::fs::write(&path, r#"{"provider":{"type":"oauth","key":"test-value"}}"#).unwrap();
        assert!(read_selected(home.path(), &field(), 0).unwrap().is_none());
    }

    #[test]
    fn rejects_corrupt_and_oversized_documents() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("auth.json");
        std::fs::write(&path, "{").unwrap();
        assert_eq!(
            discover(true, home.path(), &field()),
            vec![(0, CredentialSourceStatus::Unavailable)]
        );
        let file = std::fs::File::create(path).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        assert!(read_selected(home.path(), &field(), 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_outside_authorized_root() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), home.path().join("auth.json")).unwrap();
        assert!(read_selected(home.path(), &field(), 0).is_err());
    }
}
