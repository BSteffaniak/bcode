#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Raw Tesseract C API bindings used by bcode.
//!
//! This crate intentionally exposes only the low-level symbols required by the
//! safe `bcode_tesseract_ocr` wrapper. Build and link behavior is controlled by
//! `build.rs`; runtime OCR behavior belongs in the safe wrapper crate.

use std::ffi::{c_char, c_int, c_void};

unsafe extern "C" {
    /// Returns the linked Tesseract version string.
    pub fn TessVersion() -> *const c_char;

    /// Creates a Tesseract base API handle.
    pub fn TessBaseAPICreate() -> *mut c_void;

    /// Destroys a Tesseract base API handle.
    pub fn TessBaseAPIDelete(handle: *mut c_void);

    /// Ends the current Tesseract session and releases session resources.
    pub fn TessBaseAPIEnd(handle: *mut c_void);

    /// Initializes the Tesseract API using datapath, language, and engine mode.
    pub fn TessBaseAPIInit2(
        handle: *mut c_void,
        datapath: *const c_char,
        language: *const c_char,
        oem: c_int,
    ) -> c_int;

    /// Initializes the Tesseract API from an in-memory traineddata buffer.
    pub fn TessBaseAPIInit5(
        handle: *mut c_void,
        data: *const c_char,
        data_size: c_int,
        language: *const c_char,
        oem: c_int,
        configs: *mut *mut c_char,
        configs_size: c_int,
        vars_vec: *mut *mut c_char,
        vars_values: *mut *mut c_char,
        vars_vec_size: usize,
        set_only_non_debug_params: c_int,
    ) -> c_int;

    /// Sets raw image data for recognition.
    pub fn TessBaseAPISetImage(
        handle: *mut c_void,
        imagedata: *const u8,
        width: c_int,
        height: c_int,
        bytes_per_pixel: c_int,
        bytes_per_line: c_int,
    );

    /// Sets the page segmentation mode.
    pub fn TessBaseAPISetPageSegMode(handle: *mut c_void, mode: c_int);

    /// Sets a Tesseract configuration variable.
    pub fn TessBaseAPISetVariable(
        handle: *mut c_void,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;

    /// Runs OCR recognition on the currently configured image.
    pub fn TessBaseAPIRecognize(handle: *mut c_void, monitor: *mut c_void) -> c_int;

    /// Returns recognized UTF-8 text. The caller must free it with `TessDeleteText`.
    pub fn TessBaseAPIGetUTF8Text(handle: *mut c_void) -> *mut c_char;

    /// Frees text allocated by Tesseract.
    pub fn TessDeleteText(text: *mut c_char);
}

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_tessdata.rs"));
}

/// How the build script provided Tesseract to this crate.
///
/// * `none`: no Tesseract symbols are linked.
/// * `system`: linked against system libraries.
/// * `vendored`: statically linked from caller-provided sources or libraries.
/// * `bundled-static`: statically linked from catalog sources, with embedded tessdata.
/// * `bundled-dynamic`: catalog runtimes are loaded at runtime; no symbols are linked.
#[must_use]
pub const fn linkage() -> &'static str {
    env!("BCODE_TESSERACT_LINKAGE")
}

/// Returns traineddata files embedded by static bundled builds as
/// `(language, bytes)` pairs. Other link modes embed nothing.
#[must_use]
pub const fn embedded_tessdata() -> &'static [(&'static str, &'static [u8])] {
    embedded::EMBEDDED_TESSDATA
}

/// Returns the build-time `BCODE_TESSDATA_PREFIX` override, when one was set.
#[must_use]
pub const fn compiled_tessdata_prefix() -> Option<&'static str> {
    option_env!("BCODE_TESSDATA_PREFIX")
}

/// Returns the bundled runtime root emitted by the build script, when bundled
/// dynamic runtimes were built.
#[must_use]
pub const fn compiled_bundled_runtime_root() -> Option<&'static str> {
    option_env!("BCODE_TESSERACT_BUNDLED_RUNTIMES")
}

/// Returns the bundled versions emitted by the build script, when bundled
/// runtimes were built or statically linked.
#[must_use]
pub const fn compiled_bundled_versions() -> Option<&'static str> {
    option_env!("BCODE_TESSERACT_BUNDLED_VERSIONS")
}
