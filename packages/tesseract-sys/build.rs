use std::env;
use std::fs;
use std::path::PathBuf;

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

    let mut embedded_tessdata = Vec::new();
    let linkage = match link_mode().as_str() {
        "none" => {
            emit_no_link_metadata();
            Linkage::None
        }
        "system" => {
            system::link();
            Linkage::System
        }
        "vendored" => {
            vendored::link();
            Linkage::Vendored
        }
        "bundled" | "download" => link_bundled(bundled_linkage(), &mut embedded_tessdata),
        "bundled-static" => link_bundled(BundledLinkage::Static, &mut embedded_tessdata),
        "bundled-dynamic" => link_bundled(BundledLinkage::Dynamic, &mut embedded_tessdata),
        other => panic!("unsupported BCODE_TESSERACT_LINK_MODE: {other}"),
    };
    write_embedded_tessdata_module(&embedded_tessdata);
    // Dependents read this as `DEP_TESSERACT_LINKAGE` to learn which symbols are linked.
    println!("cargo:linkage={}", linkage.as_str());
    println!(
        "cargo:rustc-env=BCODE_TESSERACT_LINKAGE={}",
        linkage.as_str()
    );
}

#[derive(Clone, Copy)]
enum Linkage {
    None,
    System,
    Vendored,
    BundledStatic,
    BundledDynamic,
}

impl Linkage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::System => "system",
            Self::Vendored => "vendored",
            Self::BundledStatic => "bundled-static",
            Self::BundledDynamic => "bundled-dynamic",
        }
    }
}

#[derive(Clone, Copy)]
enum BundledLinkage {
    Static,
    Dynamic,
}

fn bundled_linkage() -> BundledLinkage {
    if cfg!(feature = "bundled-tesseract-dynamic") {
        BundledLinkage::Dynamic
    } else {
        BundledLinkage::Static
    }
}

fn link_bundled(
    linkage: BundledLinkage,
    embedded_tessdata: &mut Vec<(String, PathBuf)>,
) -> Linkage {
    validate_bundled_selection();
    match linkage {
        BundledLinkage::Static => {
            let versions = selected_bundled_versions();
            assert!(
                versions.len() == 1,
                "static bundled Tesseract links exactly one version, but {} were selected ({}); enable bundled-tesseract-dynamic to bundle several versions as loadable runtimes",
                versions.len(),
                versions.join(", ")
            );
            *embedded_tessdata = bundled::link_static(&versions[0]);
            Linkage::BundledStatic
        }
        BundledLinkage::Dynamic => {
            bundled::link_dynamic();
            Linkage::BundledDynamic
        }
    }
}

/// Writes the Rust module that embeds traineddata files into the crate.
///
/// Every link mode writes the module so the crate always compiles; only static
/// bundled builds populate it.
fn write_embedded_tessdata_module(entries: &[(String, PathBuf)]) {
    let mut module = String::from("pub static EMBEDDED_TESSDATA: &[(&str, &[u8])] = &[\n");
    for (language, path) in entries {
        let path = path.to_str().unwrap_or_else(|| {
            panic!(
                "embedded tessdata path is not valid UTF-8: {}",
                path.display()
            )
        });
        module.push_str(&format!(
            "    ({language:?}, include_bytes!({path:?}).as_slice()),\n"
        ));
    }
    module.push_str("];\n");
    let destination = out_dir().join("embedded_tessdata.rs");
    if fs::read_to_string(&destination).ok().as_deref() != Some(module.as_str()) {
        fs::write(&destination, module).unwrap_or_else(|error| {
            panic!(
                "failed to write embedded tessdata module {}: {error}",
                destination.display()
            )
        });
    }
}

fn out_dir() -> PathBuf {
    PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"))
}

/// Build scripts run on the host, so target properties come from Cargo's
/// `CARGO_CFG_*` variables rather than `cfg!`.
fn target_os_is(os: &str) -> bool {
    env::var("CARGO_CFG_TARGET_OS").is_ok_and(|value| value == os)
}

fn target_env_is(target_env: &str) -> bool {
    env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|value| value == target_env)
}

fn emit_tessdata_prefix_override() {
    if let Ok(prefix) = env::var("BCODE_TESSDATA_PREFIX") {
        println!("cargo:rustc-env=BCODE_TESSDATA_PREFIX={prefix}");
    }
}

fn link_mode() -> String {
    env::var("BCODE_TESSERACT_LINK_MODE").unwrap_or_else(|_| default_link_mode())
}

fn default_link_mode() -> String {
    let bundled = cfg!(feature = "bundled-tesseract") || !selected_bundled_versions().is_empty();
    let system = cfg!(feature = "system-tesseract");

    match (bundled, system) {
        (true, false) => "bundled".to_string(),
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

fn emit_no_link_metadata() {
    println!("cargo:rustc-env=BCODE_TESSERACT_LINK_MODE=none");
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

mod system {
    pub(super) fn link() {
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
}

mod vendored {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

    pub(super) fn link() {
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
        build_and_link_static(
            &leptonica_source,
            &tesseract_source,
            &super::out_dir().join("vendored"),
        );
    }

    /// Builds Leptonica and Tesseract as static libraries under `build_root`
    /// and links both into the current crate.
    ///
    /// Optional codec, network, archive, and graphics integrations are
    /// disabled so the static archives need no further system libraries.
    pub(super) fn build_and_link_static(
        leptonica_source: &Path,
        tesseract_source: &Path,
        build_root: &Path,
    ) {
        let leptonica_install = cmake::Config::new(leptonica_source)
            .out_dir(build_root.join("leptonica"))
            .profile("Release")
            // Leptonica reads this as a preprocessor define (there is no CMake
            // option); it silences diagnostics such as missing optional codecs.
            .cflag("-DNO_CONSOLE_IO")
            .define("leptonica_INSTALL_CMAKE_DIR", "lib/cmake/leptonica")
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .define("CMAKE_INSTALL_LIBDIR", "lib")
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
            .define("SW_BUILD", "OFF")
            .define("HAVE_LIBZ", "0")
            .build();

        let leptonica_include = leptonica_install.join("include");
        let leptonica_lib_dir = leptonica_install.join("lib");
        let leptonica_library = find_static_library(&leptonica_lib_dir, "leptonica");

        let tesseract_install = cmake::Config::new(tesseract_source)
            .out_dir(build_root.join("tesseract"))
            .profile("Release")
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .define("CMAKE_INSTALL_LIBDIR", "lib")
            .define("CMAKE_BUILD_TYPE", "Release")
            .define("BUILD_SHARED_LIBS", "OFF")
            .define("BUILD_TRAINING_TOOLS", "OFF")
            .define("BUILD_TESTS", "OFF")
            .define("BUILD_PROG", "OFF")
            .define("DISABLED_LEGACY_ENGINE", "OFF")
            .define("GRAPHICS_DISABLED", "ON")
            .define("DISABLE_ARCHIVE", "ON")
            .define("DISABLE_CURL", "ON")
            .define("DISABLE_TIFF", "ON")
            .define("INSTALL_CONFIGS", "OFF")
            .define("USE_OPENCL", "OFF")
            .define("OPENMP_BUILD", "OFF")
            .define("SW_BUILD", "OFF")
            .define("ENABLE_CCACHE", "OFF")
            .define(
                "Leptonica_DIR",
                leptonica_install.join("lib/cmake/leptonica"),
            )
            // Clear a cached pkg-config failure when reconfiguring after discovery.
            .configure_arg("-ULeptonica_FOUND")
            .define("Leptonica_INCLUDE_DIRS", &leptonica_include)
            .define("Leptonica_LIBRARIES", &leptonica_library)
            .build();

        let tesseract_library = find_static_library(&tesseract_install.join("lib"), "tesseract");
        link_static_library(&tesseract_library);
        link_static_library(&leptonica_library);
        link_cpp_runtime();
        if super::target_os_is("windows") {
            println!("cargo:rustc-link-lib=ws2_32");
        }
    }

    fn env_path(name: &str) -> PathBuf {
        env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{name} must be set when BCODE_TESSERACT_LINK_MODE=vendored"))
    }

    fn static_library_extension() -> &'static str {
        if super::target_env_is("msvc") {
            ".lib"
        } else {
            ".a"
        }
    }

    /// Finds the installed static archive for `name`, whose file name may carry
    /// a platform prefix and version suffix (for example `tesseract55.lib`).
    fn find_static_library(lib_dir: &Path, name: &str) -> PathBuf {
        let extension = static_library_extension();
        [lib_dir.to_path_buf(), lib_dir.join("Release")]
            .into_iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|file_name| file_name.to_str())
                    .is_some_and(|file_name| {
                        file_name.contains(name) && file_name.ends_with(extension)
                    })
            })
            .min()
            .unwrap_or_else(|| {
                panic!(
                    "failed to find static library {name} under {}",
                    lib_dir.display()
                )
            })
    }

    fn link_static_library(library: &Path) {
        let directory = library
            .parent()
            .expect("static library path must have a parent directory");
        let file_name = library
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .expect("static library file name must be UTF-8");
        let stem = file_name
            .strip_suffix(static_library_extension())
            .expect("static library has the platform extension");
        let link_name = if super::target_env_is("msvc") {
            stem
        } else {
            stem.strip_prefix("lib").unwrap_or(stem)
        };
        println!("cargo:rustc-link-search=native={}", directory.display());
        println!("cargo:rustc-link-lib=static={link_name}");
    }

    fn link_cpp_runtime() {
        if super::target_os_is("macos") || super::target_os_is("ios") {
            println!("cargo:rustc-link-lib=c++");
        } else if super::target_env_is("msvc") {
            // MSVC links the C++ runtime through compiler defaults.
        } else {
            println!("cargo:rustc-link-lib=stdc++");
        }
    }
}

#[cfg(feature = "bundled-tesseract-build")]
mod bundled {
    use std::env;
    use std::fs;
    use std::io::{self, Cursor};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    const BUNDLED_CATALOG_TOML: &str = include_str!("bundled/catalog.generated.toml");

    use super::selected_bundled_versions;

    struct TesseractCatalogEntry {
        version: String,
        url: String,
        sha256: String,
        leptonica: String,
    }

    struct LeptonicaCatalogEntry<'a> {
        url: &'a str,
        sha256: &'a str,
    }

    struct TessdataLanguageEntry<'a> {
        code: &'a str,
        url: &'a str,
        sha256: &'a str,
    }

    struct BundledCatalog {
        value: toml::Value,
    }

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

    fn required_str<'a>(value: &'a toml::Value, key: &str) -> &'a str {
        value
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("catalog field {key} is required"))
    }

    /// Builds one catalog version as static libraries, links it into the
    /// crate, and returns the traineddata files to embed.
    pub(super) fn link_static(version: &str) -> Vec<(String, PathBuf)> {
        let catalog = BundledCatalog::load();
        let sources_dir = out_dir().join("sources");
        fs::create_dir_all(&sources_dir).expect("failed to create bundled source directory");
        let (leptonica_source, tesseract_source) =
            download_sources(&catalog, version, &sources_dir);
        super::vendored::build_and_link_static(
            &leptonica_source,
            &tesseract_source,
            &out_dir().join(format!("static-{version}")),
        );
        emit_bundled_version_metadata(&catalog, &[version.to_owned()]);
        if env::var_os("BCODE_TESSDATA_PREFIX").is_some() {
            return Vec::new();
        }
        let tessdata_dir = out_dir().join("embedded-tessdata");
        fs::create_dir_all(&tessdata_dir).expect("failed to create embedded tessdata directory");
        catalog
            .tessdata_languages()
            .into_iter()
            .map(|language| {
                let path = tessdata_dir.join(format!("{}.traineddata", language.code));
                download_file_to(language.url, language.sha256, &path);
                (language.code.to_owned(), path)
            })
            .collect()
    }

    /// Builds every selected catalog version as a loadable runtime tree with
    /// its own tessdata directory.
    pub(super) fn link_dynamic() {
        let catalog = BundledCatalog::load();
        let versions = selected_bundled_versions();
        let runtimes_dir = out_dir().join("bundled-runtimes");
        let sources_dir = out_dir().join("sources");
        fs::create_dir_all(&runtimes_dir).expect("failed to create bundled runtime directory");
        fs::create_dir_all(&sources_dir).expect("failed to create bundled source directory");

        for version in &versions {
            let tesseract = catalog.tesseract(version);
            let (leptonica_source, tesseract_source) =
                download_sources(&catalog, version, &sources_dir);
            build_runtime(
                &tesseract,
                &leptonica_source,
                &tesseract_source,
                &runtimes_dir,
            );
        }
        download_runtime_tessdata(&catalog, &versions, &runtimes_dir);
        emit_bundled_version_metadata(&catalog, &versions);
    }

    fn download_sources(
        catalog: &BundledCatalog,
        version: &str,
        sources_dir: &Path,
    ) -> (PathBuf, PathBuf) {
        let tesseract = catalog.tesseract(version);
        let leptonica = catalog.leptonica(&tesseract.leptonica);
        let leptonica_source = download_and_extract(
            leptonica.url,
            leptonica.sha256,
            sources_dir,
            &format!("leptonica-{}", tesseract.leptonica),
        );
        let tesseract_source = download_and_extract(
            &tesseract.url,
            &tesseract.sha256,
            sources_dir,
            &format!("tesseract-{}", tesseract.version),
        );
        (leptonica_source, tesseract_source)
    }

    fn emit_bundled_version_metadata(catalog: &BundledCatalog, versions: &[String]) {
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

    fn build_runtime(
        tesseract: &TesseractCatalogEntry,
        leptonica_source: &Path,
        tesseract_source: &Path,
        runtimes_dir: &Path,
    ) {
        let runtime_dir = runtimes_dir.join(&tesseract.version);
        if let Ok(metadata) = fs::symlink_metadata(&runtime_dir)
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            panic!(
                "refusing unsafe bundled runtime destination {}",
                runtime_dir.display()
            );
        }
        let runtime_lib = runtime_dir.join("lib");
        fs::create_dir_all(&runtime_lib).expect("failed to create bundled runtime lib directory");
        let canonical_runtime = runtime_dir
            .canonicalize()
            .expect("failed to canonicalize bundled runtime directory");
        let canonical_lib = runtime_lib
            .canonicalize()
            .expect("failed to canonicalize bundled runtime library directory");
        assert!(
            canonical_lib.starts_with(&canonical_runtime),
            "bundled runtime library directory escapes its runtime root"
        );

        let leptonica_install = cmake::Config::new(leptonica_source)
            .define("leptonica_INSTALL_CMAKE_DIR", "lib/cmake/leptonica")
            .out_dir(out_dir().join(format!("leptonica-build-{}", tesseract.leptonica)))
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .define("CMAKE_INSTALL_LIBDIR", "lib")
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
        let leptonica_library = if cfg!(target_os = "windows") {
            find_import_library(&leptonica_lib, "leptonica")
        } else {
            find_dynamic_library(&leptonica_lib, "leptonica")
        };

        let tesseract_install = cmake::Config::new(tesseract_source)
            .out_dir(out_dir().join(format!("tesseract-build-{}", tesseract.version)))
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .define("CMAKE_INSTALL_LIBDIR", "lib")
            .define("CMAKE_BUILD_TYPE", "Release")
            .define("BUILD_SHARED_LIBS", "ON")
            .define("BUILD_TRAINING_TOOLS", "OFF")
            .define("BUILD_TESTS", "OFF")
            .define("BUILD_PROG", "OFF")
            .define("DISABLED_LEGACY_ENGINE", "OFF")
            .define("USE_OPENCL", "OFF")
            .define("OPENMP_BUILD", "OFF")
            .define("SW_BUILD", "OFF")
            .define("CMAKE_INSTALL_RPATH", runtime_rpath())
            .define(
                "Leptonica_DIR",
                leptonica_install.join("lib/cmake/leptonica"),
            )
            // Clear a cached pkg-config failure when reconfiguring after discovery.
            .configure_arg("-ULeptonica_FOUND")
            .define("Leptonica_INCLUDE_DIRS", &leptonica_include)
            .define("Leptonica_LIBRARIES", &leptonica_library)
            .build();

        copy_runtime_libraries(&leptonica_lib, &runtime_lib, "leptonica");
        copy_runtime_libraries(&tesseract_install.join("lib"), &runtime_lib, "tesseract");
        patch_runtime_libraries(&runtime_lib);
    }

    fn runtime_rpath() -> &'static str {
        if cfg!(target_os = "macos") {
            "@loader_path"
        } else if cfg!(target_os = "windows") {
            ""
        } else {
            "$ORIGIN"
        }
    }

    fn find_dynamic_library(lib_dir: &Path, name: &str) -> PathBuf {
        find_library_file(lib_dir, name, |file_name| {
            file_name.ends_with(".dylib")
                || file_name.contains(".so")
                || file_name.ends_with(".dll")
        })
    }

    fn find_import_library(lib_dir: &Path, name: &str) -> PathBuf {
        find_library_file(lib_dir, name, |file_name| file_name.ends_with(".lib"))
    }

    fn find_library_file(
        lib_dir: &Path,
        name: &str,
        matches_kind: impl Fn(&str) -> bool,
    ) -> PathBuf {
        let candidates = [lib_dir.to_path_buf(), lib_dir.join("Release")];
        for candidate in candidates {
            let Ok(entries) = fs::read_dir(&candidate) else {
                continue;
            };
            if let Some(path) = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|file_name| file_name.to_str())
                        .is_some_and(|file_name| {
                            file_name.contains(name) && matches_kind(file_name)
                        })
                })
            {
                return path;
            }
        }
        panic!("failed to find library {name} in {}", lib_dir.display());
    }

    fn copy_runtime_libraries(source_lib_dir: &Path, runtime_lib: &Path, name: &str) {
        let libraries = dynamic_libraries(source_lib_dir, name);
        assert!(
            !libraries.is_empty(),
            "failed to find runtime library {name} under {}",
            source_lib_dir.display()
        );
        for path in &libraries {
            fs::copy(
                path,
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
        if cfg!(target_os = "windows") {
            let canonical = runtime_lib.join(dynamic_library_name(name));
            if !canonical.is_file() {
                fs::copy(&libraries[0], &canonical).unwrap_or_else(|error| {
                    panic!(
                        "failed to create canonical Windows runtime library {} from {}: {error}",
                        canonical.display(),
                        libraries[0].display()
                    )
                });
            }
        }
    }

    fn dynamic_libraries(lib_dir: &Path, name: &str) -> Vec<PathBuf> {
        let mut search_dirs = vec![lib_dir.to_path_buf()];
        if cfg!(target_os = "windows")
            && let Some(install_dir) = lib_dir.parent()
        {
            search_dirs.extend([
                lib_dir.join("Release"),
                install_dir.join("bin"),
                install_dir.join("bin").join("Release"),
            ]);
        }
        let mut libraries: Vec<_> = search_dirs
            .into_iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|file_name| file_name.to_str())
                    .is_some_and(|file_name| {
                        file_name.contains(name)
                            && (file_name.ends_with(".dylib")
                                || file_name.contains(".so")
                                || file_name.to_ascii_lowercase().ends_with(".dll"))
                    })
            })
            .collect();
        libraries.sort();
        libraries.dedup();
        libraries
    }

    fn patch_runtime_libraries(runtime_lib: &Path) {
        if cfg!(target_os = "macos") {
            patch_macos_runtime_libraries(runtime_lib);
        } else if cfg!(target_os = "linux") {
            validate_linux_runtime_libraries(runtime_lib);
        }
    }

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

    fn patch_id(library: &Path, id: &str) {
        run_install_name_tool(library, &["-id", id]);
    }

    fn patch_change(library: &Path, old: &str, new: &str) {
        run_install_name_tool(library, &["-change", old, new]);
    }

    fn patch_add_rpath(library: &Path, rpath: &str) {
        if otool_contains(library, rpath) {
            return;
        }
        run_install_name_tool(library, &["-add_rpath", rpath]);
    }

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

    fn otool_contains(library: &Path, needle: &str) -> bool {
        let output = Command::new("otool")
            .arg("-l")
            .arg(library)
            .output()
            .or_else(|_| Command::new("otool").arg("-L").arg(library).output())
            .unwrap_or_else(|error| {
                panic!("failed to run otool on {}: {error}", library.display())
            });
        if !output.status.success() {
            panic!("otool failed for {}", library.display());
        }
        String::from_utf8_lossy(&output.stdout).contains(needle)
    }

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

    fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn dynamic_library_name(name: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("lib{name}.dylib")
        } else if cfg!(target_os = "windows") {
            format!("{name}.dll")
        } else {
            format!("lib{name}.so")
        }
    }

    fn download_and_extract(
        url: &str,
        expected_sha256: &str,
        target_dir: &Path,
        name: &str,
    ) -> PathBuf {
        let extracted = target_dir.join(name);
        if valid_source_dir(&extracted) {
            return extracted;
        }
        if extracted.exists() {
            fs::remove_dir_all(&extracted).expect("failed to remove incomplete source directory");
        }

        let source_cache = source_cache_dir().join(expected_sha256);
        if valid_source_dir(&source_cache) {
            copy_dir_all(&source_cache, &extracted)
                .expect("failed to copy cached source directory");
            return extracted;
        }
        if source_cache.exists() {
            fs::remove_dir_all(&source_cache).expect("failed to remove incomplete source cache");
        }

        let bytes = download_verified(url, expected_sha256);
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).expect("failed to open source zip");
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

    fn valid_source_dir(path: &Path) -> bool {
        path.join("CMakeLists.txt").is_file() && path.join("src").is_dir()
    }

    fn download_runtime_tessdata(
        catalog: &BundledCatalog,
        versions: &[String],
        runtimes_dir: &Path,
    ) {
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

    fn artifact_cache_path(url: &str, expected_sha256: &str) -> PathBuf {
        artifact_cache_dir().join(format!("{}{}", expected_sha256, artifact_extension(url)))
    }

    fn artifact_extension(url: &str) -> &'static str {
        if url.ends_with(".zip") {
            ".zip"
        } else if url.ends_with(".traineddata") {
            ".traineddata"
        } else {
            ".bin"
        }
    }

    fn artifact_cache_dir() -> PathBuf {
        if let Some(path) = env::var_os("BCODE_TESSERACT_ARTIFACT_CACHE") {
            return PathBuf::from(path);
        }
        cache_root().join("tesseract-artifacts")
    }

    fn source_cache_dir() -> PathBuf {
        if let Some(path) = env::var_os("BCODE_TESSERACT_SOURCE_CACHE") {
            return PathBuf::from(path);
        }
        cache_root().join("tesseract-sources")
    }

    fn cache_root() -> PathBuf {
        if let Some(path) = env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(path).join("bcode");
        }
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(".cache").join("bcode");
        }
        out_dir().join("cache")
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        hex::encode(Sha256::digest(bytes))
    }

    fn out_dir() -> PathBuf {
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set"))
    }

    fn single_child_dir(path: &Path) -> Option<PathBuf> {
        let mut entries = fs::read_dir(path).ok()?.filter_map(Result::ok);
        let first = entries.next()?.path();
        if entries.next().is_none() && first.is_dir() {
            Some(first)
        } else {
            None
        }
    }

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
}

#[cfg(not(feature = "bundled-tesseract-build"))]
mod bundled {
    use std::path::PathBuf;

    pub(super) fn link_static(_version: &str) -> Vec<(String, PathBuf)> {
        panic!("BCODE_TESSERACT_LINK_MODE=bundled requires a concrete bundled Tesseract feature")
    }

    pub(super) fn link_dynamic() {
        panic!("BCODE_TESSERACT_LINK_MODE=bundled requires a concrete bundled Tesseract feature")
    }
}
