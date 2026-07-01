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

/// Returns the bundled runtime root emitted by the build script, when bundled
/// dynamic runtimes were built.
#[must_use]
pub const fn compiled_bundled_runtime_root() -> Option<&'static str> {
    option_env!("BCODE_TESSERACT_BUNDLED_RUNTIMES")
}

/// Returns the bundled versions emitted by the build script, when bundled
/// dynamic runtimes were built.
#[must_use]
pub const fn compiled_bundled_versions() -> Option<&'static str> {
    option_env!("BCODE_TESSERACT_BUNDLED_VERSIONS")
}
