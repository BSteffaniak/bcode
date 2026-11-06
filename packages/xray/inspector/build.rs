#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Compile an explicitly selected, bounded web distribution. Never runs Node or downloads.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _};
use std::path::Path;

const MAX_FILES: usize = 512;
const MAX_ENTRIES: usize = 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_PATH_BYTES: usize = 256;

type Assets = BTreeMap<String, Vec<u8>>;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn collect(root: &Path) -> io::Result<Assets> {
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err(invalid("web dist must be a real directory, not a symlink"));
    }
    let root = root.canonicalize()?;
    let mut assets = Assets::new();
    let mut budget = (0, 0);
    visit(&root, &root, 0, &mut assets, &mut budget)?;
    if assets.get("/index.html").is_none_or(Vec::is_empty)
        || !assets.iter().any(|(name, bytes)| {
            Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("js"))
                && !bytes.is_empty()
        })
    {
        return Err(invalid(
            "web dist needs nonempty index.html and built JavaScript; run npm run build in packages/xray/web",
        ));
    }
    Ok(assets)
}

fn visit(
    root: &Path,
    directory: &Path,
    depth: usize,
    assets: &mut Assets,
    budget: &mut (usize, usize),
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(invalid("web dist exceeds depth limit (8)"));
    }
    println!("cargo:rerun-if-changed={}", directory.display());
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        budget.0 += 1;
        if budget.0 > MAX_ENTRIES {
            return Err(invalid("web dist exceeds directory entry limit (1024)"));
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !path.canonicalize()?.starts_with(root) {
            return Err(invalid("web dist contains a symlink or escaping path"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| invalid_path(&error))?;
        let name = relative
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 asset path"))?;
        if name.len() > MAX_PATH_BYTES
            || name.split(std::path::MAIN_SEPARATOR).any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                    || part == "."
                    || part == ".."
                    || part.starts_with('.')
            })
        {
            return Err(invalid("unsafe asset path (ASCII URL-safe names required)"));
        }
        if metadata.is_dir() {
            visit(root, &path, depth + 1, assets, budget)?;
        } else if metadata.is_file() {
            if assets.len() >= MAX_FILES || metadata.len() > MAX_FILE_BYTES {
                return Err(invalid("web dist exceeds 512 files or 8 MiB per file"));
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            budget.1 += bytes.len();
            if bytes.len() as u64 > MAX_FILE_BYTES || budget.1 > MAX_TOTAL_BYTES {
                return Err(invalid("web dist exceeds 8 MiB per file or 32 MiB total"));
            }
            println!("cargo:rerun-if-changed={}", path.display());
            assets.insert(
                format!("/{}", name.replace(std::path::MAIN_SEPARATOR, "/")),
                bytes,
            );
        } else {
            return Err(invalid("web dist contains a non-regular file"));
        }
    }
    Ok(())
}

fn invalid_path(error: &std::path::StripPrefixError) -> io::Error {
    invalid(error.to_string())
}

fn generate() -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=BCODE_XRAY_WEB_DIST");
    println!("cargo:rerun-if-env-changed=BCODE_DISTRIBUTION_BUILD");
    let assets = match env::var_os("BCODE_XRAY_WEB_DIST") {
        Some(path) => collect(Path::new(&path))?,
        _ if env::var_os("BCODE_DISTRIBUTION_BUILD").is_some_and(|value| value == "1") => {
            return Err(invalid(
                "distribution requires BCODE_XRAY_WEB_DIST; use cargo xtask release to build and bundle X-ray assets",
            ));
        }
        _ => Assets::new(),
    };
    let out = env::var_os("OUT_DIR").ok_or_else(|| invalid("missing OUT_DIR"))?;
    let out = Path::new(&out);
    let mut manifest = String::from("&[\n");
    // Snapshot bytes into OUT_DIR: rustc never reopens mutable source assets.
    for (index, (name, bytes)) in assets.iter().enumerate() {
        let file = format!("xray-asset-{index}.bin");
        fs::write(out.join(&file), bytes)?;
        writeln!(
            manifest,
            "({name:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\")) as &'static [u8]),"
        )
        .map_err(|error| invalid(error.to_string()))?;
    }
    manifest.push_str("]\n");
    fs::write(out.join("xray_assets.rs"), manifest)
}

fn main() {
    if let Err(error) = generate() {
        panic!("cannot bundle X-ray web assets: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = env::temp_dir().join(format!(
                "bcode-xray-assets-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn built(&self) {
            fs::write(
                self.0.join("index.html"),
                "<script src='/assets/app.js'></script>",
            )
            .unwrap();
            fs::create_dir(self.0.join("assets")).unwrap();
            fs::write(self.0.join("assets/app.js"), b"export {};").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn snapshots_sorted_url_paths_and_exact_bytes() {
        let fixture = Fixture::new();
        fixture.built();
        let assets = collect(&fixture.0).unwrap();
        assert_eq!(
            assets.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["/assets/app.js", "/index.html"]
        );
        assert_eq!(assets["/assets/app.js"], b"export {};");
        fs::write(fixture.0.join("assets/app.js"), b"changed").unwrap();
        assert_eq!(assets["/assets/app.js"], b"export {};");
    }

    #[test]
    fn rejects_unbuilt_empty_or_unsafe_input() {
        let fixture = Fixture::new();
        assert!(collect(&fixture.0).is_err());
        fixture.built();
        fs::write(fixture.0.join("assets/app.js"), b"").unwrap();
        assert!(collect(&fixture.0).is_err());
        fs::write(fixture.0.join("assets/app.js"), b"export {};").unwrap();
        fs::write(fixture.0.join("bad%2fpath.js"), b"x").unwrap();
        assert!(collect(&fixture.0).is_err());
    }

    #[test]
    fn rejects_oversized_files_and_excessive_inventory() {
        let fixture = Fixture::new();
        fixture.built();
        let large = fixture.0.join("large.js");
        fs::File::create(&large)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(collect(&fixture.0).is_err());
        fs::remove_file(large).unwrap();
        for index in 0..MAX_FILES {
            fs::write(fixture.0.join(format!("file-{index}.js")), b"x").unwrap();
        }
        assert!(collect(&fixture.0).is_err());
    }

    #[test]
    fn rejects_total_size_and_deep_trees() {
        let fixture = Fixture::new();
        fixture.built();
        for index in 0..4 {
            fs::File::create(fixture.0.join(format!("large-{index}.js")))
                .unwrap()
                .set_len(MAX_FILE_BYTES)
                .unwrap();
        }
        assert!(collect(&fixture.0).is_err());
        let fixture = Fixture::new();
        fixture.built();
        let mut directory = fixture.0.clone();
        for _ in 0..=MAX_DEPTH {
            directory.push("nested");
            fs::create_dir(&directory).unwrap();
        }
        assert!(collect(&fixture.0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_files_and_roots() {
        let fixture = Fixture::new();
        fixture.built();
        std::os::unix::fs::symlink("index.html", fixture.0.join("linked.html")).unwrap();
        assert!(collect(&fixture.0).is_err());
        let link = Fixture::new();
        std::os::unix::fs::symlink(&fixture.0, link.0.join("dist")).unwrap();
        assert!(collect(&link.0.join("dist")).is_err());
    }
}
