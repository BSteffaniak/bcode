//! Explicit, bounded WASM distribution loading for the optional browser-local TUI.

use std::path::Path;

use hyperchad::app::AppBuilder;
use hyperchad::renderer::assets::{AssetPathTarget, StaticAssetRoute};

/// Concrete files produced by the browser TUI distribution builder.
pub const TUI_ASSET_FILES: [&str; 7] = [
    "index.html",
    "bootstrap.js",
    "browser-session.js",
    "interactive-surface.js",
    "bcode_tui_web.js",
    "bcode_tui_web_bg.wasm",
    "manifest.json",
];

const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DISTRIBUTION_BYTES: usize = 96 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserProtocol {
    session_view: u16,
    browser_bridge: u16,
    canvas: u16,
}

impl BrowserProtocol {
    const fn supported(&self) -> bool {
        self.session_view == 1 && self.browser_bridge == 1 && self.canvas == 1
    }
}

#[derive(serde::Deserialize)]
struct AssetManifest {
    schema_version: u16,
    artifact_sha256: String,
    protocol: BrowserProtocol,
    files: std::collections::BTreeMap<String, AssetDigest>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AssetDigest {
    sha256: String,
    bytes: u64,
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .flat_map(|byte| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

fn validate_manifest(files: &[(&str, Vec<u8>)]) -> Result<AssetManifest, String> {
    let manifest_bytes = files
        .iter()
        .find(|(name, _)| *name == "manifest.json")
        .map(|(_, bytes)| bytes.as_slice())
        .ok_or("missing TUI asset manifest")?;
    if manifest_bytes.len() > 64 * 1024 {
        return Err("TUI asset manifest exceeds bounds".to_owned());
    }
    let manifest: AssetManifest =
        serde_json::from_slice(manifest_bytes).map_err(|_| "invalid TUI asset manifest")?;
    if manifest.schema_version != 2
        || !manifest.protocol.supported()
        || manifest.files.len() != TUI_ASSET_FILES.len() - 1
    {
        return Err("unsupported or incomplete TUI asset manifest".to_owned());
    }
    // Match the builder's canonical sorted, compact JSON bytes (keys bytes, sha256).
    let mut canonical = std::collections::BTreeMap::new();
    for (name, bytes) in files.iter().filter(|(name, _)| *name != "manifest.json") {
        let expected = manifest
            .files
            .get(*name)
            .ok_or("missing TUI asset manifest entry")?;
        if expected.bytes != bytes.len() as u64 || expected.sha256 != sha256(bytes) {
            return Err(format!("TUI asset digest mismatch: {name}"));
        }
        canonical.insert(
            *name,
            serde_json::json!({"bytes": expected.bytes, "sha256": expected.sha256}),
        );
    }
    let canonical = serde_json::to_vec(&canonical).map_err(|_| "invalid TUI artifact identity")?;
    if manifest.artifact_sha256 != sha256(&canonical) {
        return Err("TUI artifact identity mismatch".to_owned());
    }
    Ok(manifest)
}

/// Fully loaded distribution; files cannot change between validation and serving.
#[derive(Debug)]
pub struct TuiAssets {
    files: Vec<(&'static str, Vec<u8>)>,
    compatibility: Vec<u8>,
}

impl TuiAssets {
    /// Read and validate every required file before daemon or listener initialization.
    ///
    /// # Errors
    ///
    /// Returns an error for missing, empty, oversized, invalid WASM, or escaping files.
    pub fn load(directory: &Path) -> Result<Self, String> {
        use std::io::Read as _;

        let root = directory
            .canonicalize()
            .map_err(|_| "cannot open TUI assets directory")?;
        if !root.is_dir() {
            return Err("TUI assets path must be a directory".to_owned());
        }
        let mut files = Vec::with_capacity(TUI_ASSET_FILES.len());
        let mut total = 0_usize;
        for name in TUI_ASSET_FILES {
            let source = root.join(name);
            let source_metadata = std::fs::symlink_metadata(&source)
                .map_err(|_| format!("missing TUI asset: {name}"))?;
            if !source_metadata.file_type().is_file() {
                return Err(format!(
                    "TUI assets must be regular, unlinked files: {name}"
                ));
            }
            let path = source
                .canonicalize()
                .map_err(|_| format!("missing TUI asset: {name}"))?;
            if !path.starts_with(&root) {
                return Err(format!("TUI asset escapes distribution directory: {name}"));
            }
            let file =
                std::fs::File::open(path).map_err(|_| format!("cannot read TUI asset: {name}"))?;
            let metadata = file
                .metadata()
                .map_err(|_| format!("cannot inspect TUI asset: {name}"))?;
            let max_file_bytes = if name == "manifest.json" {
                64 * 1024
            } else {
                MAX_ASSET_BYTES
            };
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_file_bytes {
                return Err(format!("invalid TUI asset size or type: {name}"));
            }
            let mut bytes = Vec::new();
            file.take(max_file_bytes + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| format!("cannot read TUI asset: {name}"))?;
            total = total.saturating_add(bytes.len());
            if bytes.is_empty()
                || bytes.len() as u64 > max_file_bytes
                || total > MAX_DISTRIBUTION_BYTES
            {
                return Err(format!("TUI assets exceed distribution bounds: {name}"));
            }
            if name == "bcode_tui_web_bg.wasm" {
                if !bytes.starts_with(b"\0asm\x01\0\0\0") || bytes.len() == 8 {
                    return Err("invalid or empty TUI WASM module".to_owned());
                }
            } else if std::str::from_utf8(&bytes).is_err() {
                return Err(format!("TUI text asset is not UTF-8: {name}"));
            }
            files.push((name, bytes));
        }
        let manifest = validate_manifest(&files)?;
        let compatibility = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "artifact_sha256": manifest.artifact_sha256,
            "protocol": manifest.protocol,
        }))
        .map_err(|_| "cannot encode TUI compatibility handshake")?;
        Ok(Self {
            files,
            compatibility,
        })
    }

    /// Mount only the validated concrete files, preserving the default HTML renderer.
    #[must_use]
    pub fn with_routes(self, mut builder: AppBuilder) -> AppBuilder {
        for prefix in ["tui", "tui/v1"] {
            builder = builder.with_static_asset_route(StaticAssetRoute {
                route: format!("{prefix}/compatibility.json"),
                target: AssetPathTarget::FileContents(self.compatibility.clone().into()),
                not_found_behavior: None,
            });
        }
        for (name, bytes) in self.files {
            // The explicit .html entry gives the static renderer a truthful MIME type.
            // Mirror the concrete files here so relative bootstrap/module URLs work.
            builder = builder.with_static_asset_route(StaticAssetRoute {
                route: format!("tui/{name}"),
                target: AssetPathTarget::FileContents(bytes.clone().into()),
                not_found_behavior: None,
            });
            builder = builder.with_static_asset_route(StaticAssetRoute {
                route: format!("tui/v1/{name}"),
                target: AssetPathTarget::FileContents(bytes.into()),
                not_found_behavior: None,
            });
        }
        builder
    }
}

/// Build the browser-local TUI URL without printing its launch capability.
#[must_use]
pub fn build_tui_launch_url(address: std::net::SocketAddr, access_token: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", access_token)
        .finish();
    format!("http://{address}/tui/index.html#{query}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distribution() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let mut digests = std::collections::BTreeMap::new();
        for name in TUI_ASSET_FILES
            .into_iter()
            .filter(|name| *name != "manifest.json")
        {
            let bytes: &[u8] = if name == "bcode_tui_web_bg.wasm" {
                b"\0asm\x01\0\0\0\0\x01\0"
            } else {
                b"asset"
            };
            std::fs::write(directory.path().join(name), bytes).unwrap();
            digests.insert(
                name,
                serde_json::json!({"bytes": bytes.len(), "sha256": sha256(bytes)}),
            );
        }
        let manifest = serde_json::json!({
            "schema_version": 2,
            "protocol": {"session_view": 1, "browser_bridge": 1, "canvas": 1},
            "artifact_sha256": sha256(&serde_json::to_vec(&digests).unwrap()),
            "files": digests,
        });
        std::fs::write(
            directory.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        directory
    }

    #[test]
    fn requires_complete_nonempty_concrete_distribution() {
        let directory = distribution();
        assert!(TuiAssets::load(directory.path()).is_ok());
        std::fs::write(directory.path().join("bootstrap.js"), []).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
        std::fs::remove_file(directory.path().join("index.html")).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[test]
    fn rejects_manifest_digest_size_and_version_mismatches() {
        let directory = distribution();
        std::fs::write(directory.path().join("bootstrap.js"), b"other").unwrap();
        assert!(
            TuiAssets::load(directory.path())
                .unwrap_err()
                .contains("digest mismatch")
        );
        let directory = distribution();
        let path = directory.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["schema_version"] = 3.into();
        std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[test]
    fn rejects_mixed_protocols_and_missing_handshake() {
        for field in ["session_view", "browser_bridge", "canvas"] {
            let directory = distribution();
            let path = directory.path().join("manifest.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest["protocol"][field] = 2.into();
            std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            assert!(TuiAssets::load(directory.path()).is_err(), "{field}");
        }
        let directory = distribution();
        let path = directory.path().join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest.as_object_mut().unwrap().remove("protocol");
        std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[test]
    fn compatibility_is_derived_from_verified_bytes() {
        let directory = distribution();
        let loaded = TuiAssets::load(directory.path()).unwrap();
        let handshake: serde_json::Value = serde_json::from_slice(&loaded.compatibility).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(handshake["artifact_sha256"], manifest["artifact_sha256"]);
        assert_eq!(handshake["protocol"], manifest["protocol"]);
        assert_eq!(handshake["schema_version"], 1);
        std::fs::write(
            directory.path().join("browser-session.js"),
            b"mixed release",
        )
        .unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[test]
    fn rejects_invalid_wasm_and_oversized_assets() {
        let directory = distribution();
        std::fs::write(directory.path().join("bcode_tui_web_bg.wasm"), b"not wasm").unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
        let file = std::fs::File::create(directory.path().join("bootstrap.js")).unwrap();
        file.set_len(MAX_ASSET_BYTES + 1).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_assets_linked_outside_distribution() {
        let directory = distribution();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(directory.path().join("bootstrap.js")).unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("bootstrap.js")).unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_links_even_within_distribution() {
        let directory = distribution();
        std::fs::remove_file(directory.path().join("bootstrap.js")).unwrap();
        std::os::unix::fs::symlink("browser-session.js", directory.path().join("bootstrap.js"))
            .unwrap();
        assert!(TuiAssets::load(directory.path()).is_err());
    }

    #[test]
    fn launch_url_encodes_capability_and_uses_tui_entry() {
        assert_eq!(
            build_tui_launch_url("127.0.0.1:1234".parse().unwrap(), "a&b"),
            "http://127.0.0.1:1234/tui/index.html#token=a%26b"
        );
    }
}
