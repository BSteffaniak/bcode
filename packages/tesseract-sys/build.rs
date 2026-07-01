use std::env;
use std::path::{Path, PathBuf};

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
use std::{
    fs,
    io::{self, Cursor},
};

#[cfg(feature = "bundled-tesseract-v5-3-4")]
const BUNDLED_TESSERACT_VERSION_5_3_4: bool = true;
#[cfg(not(feature = "bundled-tesseract-v5-3-4"))]
const BUNDLED_TESSERACT_VERSION_5_3_4: bool = false;

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
const BUNDLED_CATALOG_TOML: &str = include_str!("bundled/catalog.toml");

fn main() {
    println!("cargo:rerun-if-changed=bundled/catalog.toml");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_LINK_MODE");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_LIB_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_LIB_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSDATA_PREFIX");
    emit_tessdata_prefix_override();

    let mode = env::var("BCODE_TESSERACT_LINK_MODE").unwrap_or_else(|_| default_link_mode());
    match mode.as_str() {
        "system" => link_system(),
        "vendored" => link_vendored(),
        "bundled" | "download" => link_bundled(),
        other => panic!("unsupported BCODE_TESSERACT_LINK_MODE: {other}"),
    }
}

fn emit_tessdata_prefix_override() {
    if let Ok(prefix) = env::var("BCODE_TESSDATA_PREFIX") {
        println!("cargo:rustc-env=BCODE_TESSDATA_PREFIX={prefix}");
    }
}

fn default_link_mode() -> String {
    let bundled = cfg!(feature = "bundled-tesseract") || !selected_bundled_versions().is_empty();
    let system = cfg!(feature = "system-tesseract");

    match (bundled, system) {
        (true, false) => {
            validate_bundled_selection();
            "bundled".to_string()
        }
        (false, true) => "system".to_string(),
        (false, false) => panic!(
            "no Tesseract link mode selected; enable either the system-tesseract or bundled-tesseract feature, or set BCODE_TESSERACT_LINK_MODE"
        ),
        (true, true) => panic!(
            "both system-tesseract and bundled-tesseract features are enabled; choose one, or set BCODE_TESSERACT_LINK_MODE explicitly"
        ),
    }
}

fn validate_bundled_selection() {
    if selected_bundled_versions().is_empty() {
        panic!(
            "bundled-tesseract was enabled without a concrete bundled Tesseract version; enable bundled-tesseract-default, bundled-tesseract-latest, or a bundled-tesseract-v* feature"
        );
    }
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
#[derive(Clone, Debug)]
struct TesseractCatalogEntry {
    version: String,
    url: String,
    sha256: String,
    leptonica: String,
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
#[derive(Clone, Copy, Debug)]
struct LeptonicaCatalogEntry<'a> {
    url: &'a str,
    sha256: &'a str,
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
#[derive(Clone, Copy, Debug)]
struct TessdataLanguageEntry<'a> {
    code: &'a str,
    url: &'a str,
    sha256: &'a str,
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
#[derive(Debug)]
struct BundledCatalog {
    value: toml::Value,
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
impl BundledCatalog {
    fn load() -> Self {
        Self {
            value: toml::from_str(BUNDLED_CATALOG_TOML)
                .expect("failed to parse bundled Tesseract catalog"),
        }
    }

    fn selected_tesseract(&self) -> TesseractCatalogEntry {
        let versions = selected_bundled_versions();
        let version = default_selected_version(&versions, self);
        self.tesseract(version)
    }

    fn tesseract(&self, version: &str) -> TesseractCatalogEntry {
        let entry = self
            .value
            .get("tesseract")
            .and_then(|value| value.get(version))
            .unwrap_or_else(|| panic!("bundled Tesseract version {version} is not in catalog"));
        TesseractCatalogEntry {
            version: version.to_string(),
            url: required_str(entry, "url").to_string(),
            sha256: required_str(entry, "sha256").to_string(),
            leptonica: required_str(entry, "leptonica").to_string(),
        }
    }

    fn leptonica(&self, version: &str) -> LeptonicaCatalogEntry<'_> {
        let entry = self
            .value
            .get("leptonica")
            .and_then(|value| value.get(version))
            .unwrap_or_else(|| panic!("Leptonica version {version} is not in catalog"));
        LeptonicaCatalogEntry {
            url: required_str(entry, "url"),
            sha256: required_str(entry, "sha256"),
        }
    }

    fn tessdata_languages(&self) -> Vec<TessdataLanguageEntry<'_>> {
        let languages = self
            .value
            .get("tessdata")
            .and_then(|value| value.get("best"))
            .and_then(|value| value.get("languages"))
            .and_then(toml::Value::as_table)
            .expect("catalog tessdata.best.languages table is required");
        languages
            .iter()
            .map(|(code, entry)| TessdataLanguageEntry {
                code,
                url: required_str(entry, "url"),
                sha256: required_str(entry, "sha256"),
            })
            .collect()
    }

    fn alias(&self, name: &str) -> &str {
        self.value
            .get("aliases")
            .and_then(|value| value.get(name))
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("catalog alias {name} is required"))
    }
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn default_selected_version<'a>(versions: &'a [&'a str], catalog: &BundledCatalog) -> &'a str {
    let default = catalog.alias("default");
    if versions.contains(&default) {
        return versions
            .iter()
            .copied()
            .find(|version| *version == default)
            .expect("default version should be selected");
    }
    let latest = catalog.alias("latest");
    if versions.contains(&latest) {
        return versions
            .iter()
            .copied()
            .find(|version| *version == latest)
            .expect("latest version should be selected");
    }
    versions[0]
}

fn selected_bundled_versions() -> Vec<&'static str> {
    let mut versions = Vec::new();
    if BUNDLED_TESSERACT_VERSION_5_3_4 {
        versions.push("5.3.4");
    }
    versions
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn required_str<'a>(value: &'a toml::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("catalog field {key} is required"))
}

fn link_system() {
    if let Ok(library) = pkg_config::Config::new()
        .atleast_version("5")
        .probe("tesseract")
    {
        for path in library.include_paths {
            println!("cargo:include={}", path.display());
        }
        return;
    }

    println!("cargo:rustc-link-lib=tesseract");
    println!("cargo:rustc-link-lib=leptonica");
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn link_bundled() {
    let catalog = BundledCatalog::load();
    let tesseract = catalog.selected_tesseract();
    let leptonica = catalog.leptonica(&tesseract.leptonica);
    let sources_dir = out_dir().join("sources");
    fs::create_dir_all(&sources_dir).expect("failed to create bundled source directory");

    let leptonica_source = download_and_extract(
        leptonica.url,
        leptonica.sha256,
        &sources_dir,
        &format!("leptonica-{}", tesseract.leptonica),
    );
    let tesseract_source = download_and_extract(
        &tesseract.url,
        &tesseract.sha256,
        &sources_dir,
        &format!("tesseract-{}", tesseract.version),
    );
    if env::var_os("BCODE_TESSDATA_PREFIX").is_none() {
        download_tessdata(&catalog);
    }

    build_and_link(&leptonica_source, &tesseract_source);
}

#[cfg(not(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4")))]
fn link_bundled() {
    panic!("BCODE_TESSERACT_LINK_MODE=bundled requires the bundled-tesseract feature")
}

fn link_vendored() {
    if let (Ok(tesseract_lib_dir), Ok(leptonica_lib_dir)) = (
        env::var("BCODE_TESSERACT_LIB_DIR"),
        env::var("BCODE_LEPTONICA_LIB_DIR"),
    ) {
        println!("cargo:rustc-link-search=native={tesseract_lib_dir}");
        println!("cargo:rustc-link-search=native={leptonica_lib_dir}");
        println!("cargo:rustc-link-lib=static=tesseract");
        println!("cargo:rustc-link-lib=static=leptonica");
        link_cpp_runtime();
        return;
    }

    let leptonica_source = env_path("BCODE_LEPTONICA_SOURCE_DIR");
    let tesseract_source = env_path("BCODE_TESSERACT_SOURCE_DIR");
    build_and_link(&leptonica_source, &tesseract_source);
}

fn build_and_link(leptonica_source: &Path, tesseract_source: &Path) {
    let leptonica_install = cmake::Config::new(leptonica_source)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("BUILD_PROG", "OFF")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("ENABLE_ZLIB", "OFF")
        .define("ENABLE_PNG", "OFF")
        .define("ENABLE_JPEG", "OFF")
        .define("ENABLE_TIFF", "OFF")
        .define("ENABLE_WEBP", "OFF")
        .define("ENABLE_OPENJPEG", "OFF")
        .define("ENABLE_GIF", "OFF")
        .define("NO_CONSOLE_IO", "ON")
        .define("SW_BUILD", "OFF")
        .define("HAVE_LIBZ", "0")
        .build();

    let leptonica_include = leptonica_install.join("include");
    let leptonica_lib = leptonica_install.join("lib");

    let tesseract_install = cmake::Config::new(tesseract_source)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_TRAINING_TOOLS", "OFF")
        .define("BUILD_TESTS", "OFF")
        .define("BUILD_PROG", "OFF")
        .define("DISABLED_LEGACY_ENGINE", "OFF")
        .define("USE_OPENCL", "OFF")
        .define("OPENMP_BUILD", "OFF")
        .define("Leptonica_INCLUDE_DIRS", &leptonica_include)
        .define(
            "Leptonica_LIBRARIES",
            leptonica_lib.join(static_library_name("leptonica")),
        )
        .build();

    println!(
        "cargo:rustc-link-search=native={}",
        tesseract_install.join("lib").display()
    );
    println!("cargo:rustc-link-search=native={}", leptonica_lib.display());
    println!("cargo:rustc-link-lib=static=tesseract");
    println!("cargo:rustc-link-lib=static=leptonica");
    link_cpp_runtime();
}

fn link_cpp_runtime() {
    if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        println!("cargo:rustc-link-lib=c++");
    } else if cfg!(target_env = "msvc") {
        // MSVC links the C++ runtime through compiler defaults.
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn download_and_extract(
    url: &str,
    expected_sha256: &str,
    target_dir: &Path,
    name: &str,
) -> PathBuf {
    let extracted = target_dir.join(name);
    if extracted.join("CMakeLists.txt").is_file() {
        return extracted;
    }

    let bytes = download_verified(url, expected_sha256);
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).expect("failed to open source zip");
    let tmp_dir = target_dir.join(format!("{name}.tmp"));
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir).expect("failed to remove stale source tmp directory");
    }
    fs::create_dir_all(&tmp_dir).expect("failed to create source tmp directory");
    archive
        .extract(&tmp_dir)
        .expect("failed to extract source zip");

    let root = single_child_dir(&tmp_dir).unwrap_or_else(|| tmp_dir.clone());
    if extracted.exists() {
        fs::remove_dir_all(&extracted).expect("failed to remove stale source directory");
    }
    fs::rename(&root, &extracted)
        .or_else(|_| copy_dir_all(&root, &extracted))
        .expect("failed to install extracted source directory");
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir).expect("failed to clean source tmp directory");
    }
    extracted
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn download_tessdata(catalog: &BundledCatalog) {
    let tessdata_dir = out_dir().join("tessdata");
    fs::create_dir_all(&tessdata_dir).expect("failed to create tessdata directory");
    for language in catalog.tessdata_languages() {
        download_file_to(
            language.url,
            language.sha256,
            &tessdata_dir.join(format!("{}.traineddata", language.code)),
        );
    }
    println!(
        "cargo:rustc-env=BCODE_TESSDATA_PREFIX={}",
        tessdata_dir.display()
    );
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn download_file_to(url: &str, expected_sha256: &str, path: &Path) {
    if path.is_file() {
        let bytes = fs::read(path).expect("failed to read cached file");
        if sha256_hex(&bytes) == expected_sha256 {
            return;
        }
    }
    let bytes = download_verified(url, expected_sha256);
    fs::write(path, bytes).expect("failed to write downloaded file");
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn download_verified(url: &str, expected_sha256: &str) -> Vec<u8> {
    let bytes = reqwest::blocking::get(url)
        .unwrap_or_else(|error| panic!("failed to download {url}: {error}"))
        .error_for_status()
        .unwrap_or_else(|error| panic!("failed to download {url}: {error}"))
        .bytes()
        .unwrap_or_else(|error| panic!("failed to read {url}: {error}"))
        .to_vec();
    let actual_sha256 = sha256_hex(&bytes);
    assert_eq!(actual_sha256, expected_sha256, "sha256 mismatch for {url}");
    bytes
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    format!("{:x}", Sha256::digest(bytes))
}

fn env_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{name} must be set when BCODE_TESSERACT_LINK_MODE=vendored"))
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn out_dir() -> PathBuf {
    PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"))
}

fn static_library_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn single_child_dir(path: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(path).ok()?.filter_map(Result::ok);
    let first = entries.next()?.path();
    if entries.next().is_none() && first.is_dir() {
        Some(first)
    } else {
        None
    }
}

#[cfg(any(feature = "bundled-tesseract", feature = "bundled-tesseract-v5-3-4"))]
fn copy_dir_all(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
