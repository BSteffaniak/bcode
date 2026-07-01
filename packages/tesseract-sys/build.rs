use std::env;
use std::path::{Path, PathBuf};

#[cfg(feature = "bundled-tesseract")]
use std::{
    fs,
    io::{self, Cursor},
};

#[cfg(feature = "bundled-tesseract")]
const LEPTONICA_URL: &str =
    "https://github.com/DanBloomberg/leptonica/archive/refs/tags/1.84.1.zip";
#[cfg(feature = "bundled-tesseract")]
const LEPTONICA_SHA256: &str = "d13af21fddf8839a81ff06385e36ed51ee7442e053ab65903f2351b25904284f";
#[cfg(feature = "bundled-tesseract")]
const TESSERACT_URL: &str =
    "https://github.com/tesseract-ocr/tesseract/archive/refs/tags/5.3.4.zip";
#[cfg(feature = "bundled-tesseract")]
const TESSERACT_SHA256: &str = "01e93044f5ee7c42ded713d55ec54a44e08200237b48f9256029ba1e144b1dbc";
#[cfg(feature = "bundled-tesseract")]
const ENG_TRAINEDDATA_URL: &str =
    "https://github.com/tesseract-ocr/tessdata_best/raw/main/eng.traineddata";
#[cfg(feature = "bundled-tesseract")]
const ENG_TRAINEDDATA_SHA256: &str =
    "8280aed0782fe27257a68ea10fe7ef324ca0f8d85bd2fd145d1c2b560bcb66ba";
#[cfg(feature = "bundled-tesseract")]
const TUR_TRAINEDDATA_URL: &str =
    "https://github.com/tesseract-ocr/tessdata_best/raw/main/tur.traineddata";
#[cfg(feature = "bundled-tesseract")]
const TUR_TRAINEDDATA_SHA256: &str =
    "e0c3338dc17503dc7d335a507c9ae01b2b46cfd07561171e1e1ac55d85e8e438";

fn main() {
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
    let bundled = cfg!(feature = "bundled-tesseract");
    let system = cfg!(feature = "system-tesseract");

    match (bundled, system) {
        (true, false) => "bundled".to_string(),
        (false, true) => "system".to_string(),
        (false, false) => panic!(
            "no Tesseract link mode selected; enable either the system-tesseract or bundled-tesseract feature, or set BCODE_TESSERACT_LINK_MODE"
        ),
        (true, true) => panic!(
            "both system-tesseract and bundled-tesseract features are enabled; choose one, or set BCODE_TESSERACT_LINK_MODE explicitly"
        ),
    }
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

#[cfg(feature = "bundled-tesseract")]
fn link_bundled() {
    let sources_dir = out_dir().join("sources");
    fs::create_dir_all(&sources_dir).expect("failed to create bundled source directory");

    let leptonica_source =
        download_and_extract(LEPTONICA_URL, LEPTONICA_SHA256, &sources_dir, "leptonica");
    let tesseract_source =
        download_and_extract(TESSERACT_URL, TESSERACT_SHA256, &sources_dir, "tesseract");
    if env::var_os("BCODE_TESSDATA_PREFIX").is_none() {
        download_tessdata();
    }

    build_and_link(&leptonica_source, &tesseract_source);
}

#[cfg(not(feature = "bundled-tesseract"))]
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

#[cfg(feature = "bundled-tesseract")]
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

#[cfg(feature = "bundled-tesseract")]
fn download_tessdata() {
    let tessdata_dir = out_dir().join("tessdata");
    fs::create_dir_all(&tessdata_dir).expect("failed to create tessdata directory");
    download_file_to(
        ENG_TRAINEDDATA_URL,
        ENG_TRAINEDDATA_SHA256,
        &tessdata_dir.join("eng.traineddata"),
    );
    download_file_to(
        TUR_TRAINEDDATA_URL,
        TUR_TRAINEDDATA_SHA256,
        &tessdata_dir.join("tur.traineddata"),
    );
    println!(
        "cargo:rustc-env=BCODE_TESSDATA_PREFIX={}",
        tessdata_dir.display()
    );
}

#[cfg(feature = "bundled-tesseract")]
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

#[cfg(feature = "bundled-tesseract")]
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

#[cfg(feature = "bundled-tesseract")]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    format!("{:x}", Sha256::digest(bytes))
}

fn env_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{name} must be set when BCODE_TESSERACT_LINK_MODE=vendored"))
}

#[cfg(feature = "bundled-tesseract")]
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

#[cfg(feature = "bundled-tesseract")]
fn single_child_dir(path: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(path).ok()?.filter_map(Result::ok);
    let first = entries.next()?.path();
    if entries.next().is_none() && first.is_dir() {
        Some(first)
    } else {
        None
    }
}

#[cfg(feature = "bundled-tesseract")]
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
