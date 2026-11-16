//! Snapshot an explicitly selected browser TUI distribution into this crate.
//!
//! The distribution is built separately (`scripts/build-tui-web.sh`); this
//! script never builds it. `BCODE_TUI_WEB_BUNDLE` names the built directory.
//! Distribution builds of the HTML renderer require it, so a release binary
//! always carries its browser TUI instead of reading files beside itself.
//! Full validation (manifest digests, WASM header) happens when the embedded
//! files are loaded.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::path::Path;

include!("src/html_actix/tui_asset_files.rs");

const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn read_asset(root: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = root.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_ASSET_BYTES {
        return Err(invalid(format!(
            "browser TUI asset must be a nonempty regular file within bounds: {name}"
        )));
    }
    println!("cargo:rerun-if-changed={}", path.display());
    let mut bytes = Vec::new();
    fs::File::open(&path)?
        .take(MAX_ASSET_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn generate() -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=BCODE_TUI_WEB_BUNDLE");
    println!("cargo:rerun-if-env-changed=BCODE_DISTRIBUTION_BUILD");
    let renderer = env::var_os("CARGO_FEATURE_RENDERER_HTML_ACTIX").is_some();
    let distribution = env::var_os("BCODE_DISTRIBUTION_BUILD").is_some_and(|value| value == "1");
    let out = env::var_os("OUT_DIR").ok_or_else(|| invalid("missing OUT_DIR"))?;
    let out = Path::new(&out);
    let module = match env::var_os("BCODE_TUI_WEB_BUNDLE") {
        Some(bundle) if renderer => {
            let root = Path::new(&bundle);
            if !fs::symlink_metadata(root)?.file_type().is_dir() {
                return Err(invalid("BCODE_TUI_WEB_BUNDLE must be a real directory"));
            }
            let mut module = String::from("Some(&[\n");
            // Snapshot bytes into OUT_DIR: rustc never reopens the mutable bundle.
            for (index, name) in TUI_ASSET_FILES.into_iter().enumerate() {
                let file = format!("tui-asset-{index}.bin");
                fs::write(out.join(&file), read_asset(root, name)?)?;
                writeln!(
                    module,
                    "    ({name:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\")) as &'static [u8]),"
                )
                .map_err(|error| invalid(error.to_string()))?;
            }
            module.push_str("])\n");
            module
        }
        None if renderer && distribution => {
            return Err(invalid(
                "distribution builds with the web renderer require BCODE_TUI_WEB_BUNDLE; build it with scripts/build-tui-web.sh",
            ));
        }
        _ => "None\n".to_owned(),
    };
    fs::write(out.join("tui_assets_embedded.rs"), module)
}

fn main() {
    if let Err(error) = generate() {
        panic!("cannot embed browser TUI assets: {error}");
    }
}
