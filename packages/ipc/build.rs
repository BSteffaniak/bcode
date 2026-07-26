use sha2::{Digest as _, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const FALLBACK_INPUT: &str = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"));
const SOURCE_ROOTS: &[&str] = &["catalog", "packages", "plugins", "workers"];
const ROOT_SOURCE_FILES: &[&str] = &[
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    "clippy.toml",
];

fn main() {
    println!("cargo:rerun-if-env-changed=BCODE_BUILD_FINGERPRINT");
    if let Ok(value) = std::env::var("BCODE_BUILD_FINGERPRINT")
        && is_valid_fingerprint(&value)
    {
        println!("cargo:rustc-env=BCODE_BUILD_FINGERPRINT={value}");
        return;
    }

    let workspace_root = workspace_root();
    let source_files = source_files(&workspace_root).unwrap_or_default();
    for path in &source_files {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for root in SOURCE_ROOTS {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(root).display()
        );
    }

    let fingerprint = source_fingerprint(&workspace_root, &source_files)
        .unwrap_or_else(|| short_sha256(FALLBACK_INPUT.as_bytes()));
    println!("cargo:rustc-env=BCODE_BUILD_FINGERPRINT={fingerprint}");
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn source_files(workspace_root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = ROOT_SOURCE_FILES
        .iter()
        .map(|path| workspace_root.join(path))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    for root in SOURCE_ROOTS {
        collect_files(&workspace_root.join(root), &mut files)?;
    }
    files.sort_by(|left, right| {
        relative_path(workspace_root, left).cmp(&relative_path(workspace_root, right))
    });
    Ok(files)
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn source_fingerprint(workspace_root: &Path, files: &[PathBuf]) -> Option<String> {
    let mut digest = Sha256::new();
    for path in files {
        let relative = relative_path(workspace_root, path);
        let contents = fs::read(path).ok()?;
        update_length_prefixed(&mut digest, relative.as_bytes());
        update_length_prefixed(&mut digest, &contents);
    }
    Some(short_digest(digest.finalize().as_slice()))
}

fn relative_path(workspace_root: &Path, path: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn update_length_prefixed(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    digest.update(value);
}

fn short_sha256(input: &[u8]) -> String {
    short_digest(Sha256::digest(input).as_slice())
}

fn short_digest(digest: &[u8]) -> String {
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    hex[..16].to_string()
}

fn is_valid_fingerprint(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
