use std::env;
use std::path::{Path, PathBuf};
#[cfg(feature = "bundled-tesseract-build")]
use std::process::Command;

#[cfg(feature = "bundled-tesseract-build")]
use std::{
    fs,
    io::{self, Cursor},
    thread,
    time::Duration,
};

#[cfg(feature = "bundled-tesseract-build")]
const BUNDLED_CATALOG_TOML: &str = include_str!("bundled/catalog.generated.toml");

fn main() {
    println!("cargo:rerun-if-changed=bundled/catalog.generated.toml");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_LINK_MODE");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_LIB_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_LIB_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_LEPTONICA_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=BCODE_TESSDATA_PREFIX");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_ARTIFACT_CACHE");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_SOURCE_CACHE");
    println!("cargo:rerun-if-env-changed=BCODE_TESSERACT_OFFLINE");
    emit_tessdata_prefix_override();

    let mode = env::var("BCODE_TESSERACT_LINK_MODE").unwrap_or_else(|_| default_link_mode());
    match mode.as_str() {
        "system" => link_system(),
        "vendored" => link_vendored(),
        "bundled" | "download" => link_bundled(),
        "none" => {}
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
        (false, false) => "none".to_string(),
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

#[cfg(feature = "bundled-tesseract-build")]
#[derive(Clone, Debug)]
struct TesseractCatalogEntry {
    version: String,
    url: String,
    sha256: String,
    leptonica: String,
}

#[cfg(feature = "bundled-tesseract-build")]
#[derive(Clone, Copy, Debug)]
struct LeptonicaCatalogEntry<'a> {
    url: &'a str,
    sha256: &'a str,
}

#[cfg(feature = "bundled-tesseract-build")]
#[derive(Clone, Copy, Debug)]
struct TessdataLanguageEntry<'a> {
    code: &'a str,
    url: &'a str,
    sha256: &'a str,
}

#[cfg(feature = "bundled-tesseract-build")]
#[derive(Debug)]
struct BundledCatalog {
    value: toml::Value,
}

#[cfg(feature = "bundled-tesseract-build")]
impl BundledCatalog {
    fn load() -> Self {
        Self {
            value: toml::from_str(BUNDLED_CATALOG_TOML)
                .expect("failed to parse bundled Tesseract catalog"),
        }
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

fn selected_bundled_versions() -> Vec<String> {
    let mut versions = env::vars()
        .filter_map(|(name, value)| {
            if value != "1" {
                return None;
            }
            name.strip_prefix("CARGO_FEATURE_BUNDLED_TESSERACT_V")
                .map(|version| version.replace('_', "."))
        })
        .collect::<Vec<_>>();
    versions.sort_by_key(|version| {
        version
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    versions.dedup();
    versions
}

#[cfg(feature = "bundled-tesseract-build")]
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

#[cfg(feature = "bundled-tesseract-build")]
fn link_bundled() {
    let catalog = BundledCatalog::load();
    let versions = selected_bundled_versions();
    let runtimes_dir = out_dir().join("bundled-runtimes");
    let sources_dir = out_dir().join("sources");
    fs::create_dir_all(&runtimes_dir).expect("failed to create bundled runtime directory");
    fs::create_dir_all(&sources_dir).expect("failed to create bundled source directory");

    for version in &versions {
        let tesseract = catalog.tesseract(version);
        let leptonica = catalog.leptonica(&tesseract.leptonica);
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
        build_runtime(
            &tesseract,
            &leptonica_source,
            &tesseract_source,
            &runtimes_dir,
        );
    }
    download_runtime_tessdata(&catalog, &versions, &runtimes_dir);
    println!(
        "cargo:rustc-env=BCODE_TESSERACT_BUNDLED_RUNTIMES={}",
        runtimes_dir.display()
    );
    println!(
        "cargo:rustc-env=BCODE_TESSERACT_BUNDLED_VERSIONS={}",
        versions.join(",")
    );
    println!(
        "cargo:rustc-env=BCODE_TESSERACT_BUNDLED_DEFAULT={}",
        catalog.alias("default")
    );
    println!(
        "cargo:rustc-env=BCODE_TESSERACT_BUNDLED_LATEST={}",
        catalog.alias("latest")
    );
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1",
    feature = "bundled-tesseract-v5-5-2"
)))]
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

#[cfg(feature = "bundled-tesseract-build")]
fn build_runtime(
    tesseract: &TesseractCatalogEntry,
    leptonica_source: &Path,
    tesseract_source: &Path,
    runtimes_dir: &Path,
) {
    let runtime_dir = runtimes_dir.join(&tesseract.version);
    let runtime_lib = runtime_dir.join("lib");
    fs::create_dir_all(&runtime_lib).expect("failed to create bundled runtime lib directory");

    let leptonica_install = cmake::Config::new(leptonica_source)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("BUILD_PROG", "OFF")
        .define("BUILD_SHARED_LIBS", "ON")
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
        .define("CMAKE_INSTALL_RPATH", runtime_rpath())
        .build();

    let leptonica_include = leptonica_install.join("include");
    let leptonica_lib = leptonica_install.join("lib");
    let leptonica_library = find_dynamic_library(&leptonica_lib, "leptonica");

    let tesseract_install = cmake::Config::new(tesseract_source)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("BUILD_SHARED_LIBS", "ON")
        .define("BUILD_TRAINING_TOOLS", "OFF")
        .define("BUILD_TESTS", "OFF")
        .define("BUILD_PROG", "OFF")
        .define("DISABLED_LEGACY_ENGINE", "OFF")
        .define("USE_OPENCL", "OFF")
        .define("OPENMP_BUILD", "OFF")
        .define("CMAKE_INSTALL_RPATH", runtime_rpath())
        .define("Leptonica_INCLUDE_DIRS", &leptonica_include)
        .define("Leptonica_LIBRARIES", &leptonica_library)
        .build();

    copy_runtime_libraries(&leptonica_lib, &runtime_lib, "leptonica");
    copy_runtime_libraries(&tesseract_install.join("lib"), &runtime_lib, "tesseract");
    patch_runtime_libraries(&runtime_lib);
}

#[cfg(feature = "bundled-tesseract-build")]
fn runtime_rpath() -> &'static str {
    if cfg!(target_os = "macos") {
        "@loader_path"
    } else if cfg!(target_os = "windows") {
        ""
    } else {
        "$ORIGIN"
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn find_dynamic_library(lib_dir: &Path, name: &str) -> PathBuf {
    let expected = lib_dir.join(dynamic_library_name(name));
    if expected.exists() {
        return expected;
    }
    fs::read_dir(lib_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lib_dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .is_some_and(|file_name| {
                    file_name.contains(name)
                        && (file_name.ends_with(".dylib")
                            || file_name.ends_with(".so")
                            || file_name.ends_with(".dll"))
                })
        })
        .unwrap_or_else(|| {
            panic!(
                "failed to find dynamic library {name} in {}",
                lib_dir.display()
            )
        })
}

#[cfg(feature = "bundled-tesseract-build")]
fn copy_runtime_libraries(source_lib_dir: &Path, runtime_lib: &Path, name: &str) {
    for path in dynamic_libraries(source_lib_dir, name) {
        fs::copy(
            &path,
            runtime_lib.join(path.file_name().expect("library file name is required")),
        )
        .unwrap_or_else(|error| {
            panic!(
                "failed to copy {} to {}: {error}",
                path.display(),
                runtime_lib.display()
            )
        });
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn dynamic_libraries(lib_dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut libraries: Vec<_> = fs::read_dir(lib_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lib_dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .is_some_and(|file_name| {
                    file_name.contains(name)
                        && (file_name.ends_with(".dylib")
                            || file_name.contains(".so")
                            || file_name.ends_with(".dll"))
                })
        })
        .collect();
    libraries.sort();
    libraries.dedup();
    libraries
}

#[cfg(feature = "bundled-tesseract-build")]
fn patch_runtime_libraries(runtime_lib: &Path) {
    if cfg!(target_os = "macos") {
        patch_macos_runtime_libraries(runtime_lib);
    } else if cfg!(target_os = "linux") {
        validate_linux_runtime_libraries(runtime_lib);
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn patch_macos_runtime_libraries(runtime_lib: &Path) {
    let tesseract = runtime_lib.join(dynamic_library_name("tesseract"));
    let leptonica = runtime_lib.join(dynamic_library_name("leptonica"));
    patch_id(&tesseract, "@loader_path/libtesseract.dylib");
    patch_id(&leptonica, "@loader_path/libleptonica.dylib");
    for old_name in [
        "@rpath/libleptonica.6.dylib",
        "@rpath/libleptonica.dylib",
        "libleptonica.6.dylib",
        "libleptonica.dylib",
    ] {
        patch_change(&tesseract, old_name, "@loader_path/libleptonica.dylib");
    }
    patch_add_rpath(&tesseract, "@loader_path");
    patch_add_rpath(&leptonica, "@loader_path");
    verify_otool_contains(&tesseract, "@loader_path/libleptonica.dylib");
}

#[cfg(feature = "bundled-tesseract-build")]
fn patch_id(library: &Path, id: &str) {
    run_install_name_tool(library, &["-id", id]);
}

#[cfg(feature = "bundled-tesseract-build")]
fn patch_change(library: &Path, old: &str, new: &str) {
    run_install_name_tool(library, &["-change", old, new]);
}

#[cfg(feature = "bundled-tesseract-build")]
fn patch_add_rpath(library: &Path, rpath: &str) {
    if otool_contains(library, rpath) {
        return;
    }
    run_install_name_tool(library, &["-add_rpath", rpath]);
}

#[cfg(feature = "bundled-tesseract-build")]
fn run_install_name_tool(library: &Path, args: &[&str]) {
    let status = Command::new("install_name_tool")
        .args(args)
        .arg(library)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to run install_name_tool on {}: {error}",
                library.display()
            )
        });
    if !status.success() {
        panic!(
            "install_name_tool failed with {status} for {} args {:?}",
            library.display(),
            args
        );
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn verify_otool_contains(library: &Path, needle: &str) {
    if !otool_contains(library, needle) {
        let output = Command::new("otool")
            .arg("-L")
            .arg(library)
            .output()
            .unwrap_or_else(|error| {
                panic!("failed to run otool on {}: {error}", library.display())
            });
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "{} does not reference {needle}; otool output:\n{stdout}",
            library.display()
        );
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn otool_contains(library: &Path, needle: &str) -> bool {
    let output = Command::new("otool")
        .arg("-l")
        .arg(library)
        .output()
        .or_else(|_| Command::new("otool").arg("-L").arg(library).output())
        .unwrap_or_else(|error| panic!("failed to run otool on {}: {error}", library.display()));
    if !output.status.success() {
        panic!("otool failed for {}", library.display());
    }
    String::from_utf8_lossy(&output.stdout).contains(needle)
}

#[cfg(feature = "bundled-tesseract-build")]
fn validate_linux_runtime_libraries(runtime_lib: &Path) {
    for library in dynamic_libraries(runtime_lib, "") {
        if let Some(output) = command_stdout("readelf", &["-d", &library.display().to_string()])
            && !output.contains("$ORIGIN")
        {
            eprintln!(
                "warning: {} does not report a $ORIGIN RUNPATH/RPATH; dependent loading may require LD_LIBRARY_PATH",
                library.display()
            );
        }
    }
}

#[cfg(feature = "bundled-tesseract-build")]
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(feature = "bundled-tesseract-build")]
fn dynamic_library_name(name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else {
        format!("lib{name}.so")
    }
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

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
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

    let source_cache = source_cache_dir().join(expected_sha256);
    if source_cache.join("CMakeLists.txt").is_file() {
        copy_dir_all(&source_cache, &extracted).expect("failed to copy cached source directory");
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
    if let Err(error) = copy_dir_all(&extracted, &source_cache) {
        eprintln!(
            "warning: failed to populate source cache {}: {error}",
            source_cache.display()
        );
    }
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir).expect("failed to clean source tmp directory");
    }
    extracted
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn download_runtime_tessdata(catalog: &BundledCatalog, versions: &[String], runtimes_dir: &Path) {
    if env::var_os("BCODE_TESSDATA_PREFIX").is_some() {
        return;
    }
    for version in versions {
        let tessdata_dir = runtimes_dir.join(version).join("tessdata");
        fs::create_dir_all(&tessdata_dir).expect("failed to create tessdata directory");
        for language in catalog.tessdata_languages() {
            download_file_to(
                language.url,
                language.sha256,
                &tessdata_dir.join(format!("{}.traineddata", language.code)),
            );
        }
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn download_file_to(url: &str, expected_sha256: &str, path: &Path) {
    if path.is_file() {
        let bytes = fs::read(path).expect("failed to read cached file");
        if sha256_hex(&bytes) == expected_sha256 {
            return;
        }
        eprintln!(
            "cached artifact {} had unexpected sha256; redownloading {url}",
            path.display()
        );
    }
    let bytes = download_verified(url, expected_sha256);
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, bytes).expect("failed to write downloaded file");
    fs::rename(&tmp_path, path).unwrap_or_else(|error| {
        panic!(
            "failed to install downloaded file {} -> {}: {error}",
            tmp_path.display(),
            path.display()
        )
    });
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn download_verified(url: &str, expected_sha256: &str) -> Vec<u8> {
    let cache_path = artifact_cache_path(url, expected_sha256);
    if env::var_os("BCODE_TESSERACT_OFFLINE").is_some() {
        panic!(
            "artifact {url} is not in cache {} and BCODE_TESSERACT_OFFLINE is set",
            cache_path.display()
        );
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("bcode-tesseract-sys-build/0.0.1")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()
        .expect("failed to build HTTP client");

    let mut errors = Vec::new();
    for attempt in 1..=5 {
        match download_attempt(&client, url, expected_sha256) {
            Ok(bytes) => {
                write_cached_artifact(&cache_path, &bytes);
                return bytes;
            }
            Err(error) => {
                errors.push(format!("attempt {attempt}: {error}"));
                if attempt < 5 {
                    thread::sleep(Duration::from_secs(attempt * 2));
                }
            }
        }
    }

    panic!(
        "failed to download verified artifact {url}\ncache: {}\nexpected sha256: {expected_sha256}\n{}",
        cache_path.display(),
        errors.join("\n")
    );
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn download_attempt(
    client: &reqwest::blocking::Client,
    url: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|error| format!("request failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("HTTP error: {error}"))?;
    let bytes = response
        .bytes()
        .map_err(|error| format!("failed to read response body: {error}"))?
        .to_vec();
    let actual_sha256 = sha256_hex(&bytes);
    if actual_sha256 == expected_sha256 {
        Ok(bytes)
    } else {
        Err(format!(
            "sha256 mismatch: expected {expected_sha256}, got {actual_sha256}"
        ))
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn read_valid_cached_artifact(path: &Path, expected_sha256: &str) -> Option<Vec<u8>> {
    if !path.is_file() {
        return None;
    }
    let bytes = fs::read(path).unwrap_or_else(|error| {
        panic!("failed to read cached artifact {}: {error}", path.display())
    });
    if sha256_hex(&bytes) == expected_sha256 {
        Some(bytes)
    } else {
        eprintln!(
            "cached artifact {} had unexpected sha256; redownloading",
            path.display()
        );
        None
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn write_cached_artifact(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|error| {
            panic!(
                "failed to create artifact cache directory {}: {error}",
                parent.display()
            )
        });
    }
    let tmp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp_path, bytes).unwrap_or_else(|error| {
        panic!(
            "failed to write artifact cache file {}: {error}",
            tmp_path.display()
        )
    });
    if let Err(error) = fs::rename(&tmp_path, path) {
        if read_valid_cached_artifact(path, &sha256_hex(bytes)).is_none() {
            panic!(
                "failed to install artifact cache file {} -> {}: {error}",
                tmp_path.display(),
                path.display()
            );
        }
        let _ = fs::remove_file(&tmp_path);
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn artifact_cache_path(url: &str, expected_sha256: &str) -> PathBuf {
    artifact_cache_dir().join(format!("{}{}", expected_sha256, artifact_extension(url)))
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn artifact_extension(url: &str) -> &'static str {
    if url.ends_with(".zip") {
        ".zip"
    } else if url.ends_with(".traineddata") {
        ".traineddata"
    } else {
        ".bin"
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn artifact_cache_dir() -> PathBuf {
    if let Some(path) = env::var_os("BCODE_TESSERACT_ARTIFACT_CACHE") {
        return PathBuf::from(path);
    }
    cache_root().join("tesseract-artifacts")
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn source_cache_dir() -> PathBuf {
    if let Some(path) = env::var_os("BCODE_TESSERACT_SOURCE_CACHE") {
        return PathBuf::from(path);
    }
    cache_root().join("tesseract-sources")
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn cache_root() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("bcode");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("bcode");
    }
    out_dir().join("cache")
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    format!("{:x}", Sha256::digest(bytes))
}

fn env_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{name} must be set when BCODE_TESSERACT_LINK_MODE=vendored"))
}

#[cfg(feature = "bundled-tesseract-build")]
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

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
fn single_child_dir(path: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(path).ok()?.filter_map(Result::ok);
    let first = entries.next()?.path();
    if entries.next().is_none() && first.is_dir() {
        Some(first)
    } else {
        None
    }
}

#[cfg(feature = "bundled-tesseract-build")]
#[cfg(feature = "bundled-tesseract-build")]
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
