#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Safe, minimal Tesseract OCR wrapper used by bcode.
//!
//! The wrapper owns API handles, text allocation cleanup, runtime tessdata
//! resolution, and conversion from idiomatic Rust inputs to Tesseract's C API.

use std::env;
use std::ffi::{CStr, CString, NulError};
use std::path::PathBuf;
use std::ptr;
use thiserror::Error;

/// Errors returned by the Tesseract OCR wrapper.
#[derive(Debug, Error)]
pub enum Error {
    /// Tesseract returned a null API handle.
    #[error("failed to create tesseract API handle")]
    CreateHandle,

    /// A string passed to Tesseract contained an interior NUL byte.
    #[error("invalid string contains an interior NUL byte: {0}")]
    InvalidString(#[from] NulError),

    /// Tesseract initialization failed.
    #[error("failed to initialize tesseract for language '{language}' using tessdata '{datapath}'")]
    Init { datapath: String, language: String },

    /// Tesseract rejected a configuration variable.
    #[error("failed to set tesseract variable '{name}'")]
    SetVariable { name: String },

    /// Tesseract recognition failed.
    #[error("tesseract recognition failed")]
    Recognize,

    /// Tesseract returned a null text pointer.
    #[error("tesseract returned no recognized text")]
    Text,

    /// The requested bundled runtime was not enabled by Cargo features.
    #[error("bundled tesseract runtime '{version}' is not available in this build")]
    BundledRuntimeUnavailable { version: String },

    /// No bundled runtime was enabled by Cargo features.
    #[error("no bundled tesseract runtime is available in this build")]
    NoBundledRuntime,
}

/// Convenient result alias for Tesseract OCR operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A bundled Tesseract runtime selected at compile time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TesseractRuntime {
    version: &'static str,
}

impl TesseractRuntime {
    /// Loads the default bundled Tesseract runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if no bundled runtime is available.
    pub fn load_default() -> Result<Self> {
        let default = catalog_alias("default");
        if available_bundled_versions().contains(&default.as_str()) {
            return Ok(Self {
                version: version_static(&default),
            });
        }
        let latest = catalog_alias("latest");
        if available_bundled_versions().contains(&latest.as_str()) {
            return Ok(Self {
                version: version_static(&latest),
            });
        }
        available_bundled_versions()
            .first()
            .copied()
            .map(|version| Self { version })
            .ok_or(Error::NoBundledRuntime)
    }

    /// Loads a specific bundled Tesseract runtime by catalog version.
    ///
    /// # Errors
    ///
    /// Returns an error if the version was not selected by Cargo features.
    pub fn load_version(version: &str) -> Result<Self> {
        if let Some(version) = available_bundled_versions()
            .iter()
            .copied()
            .find(|available| *available == version)
        {
            return Ok(Self { version });
        }
        Err(Error::BundledRuntimeUnavailable {
            version: version.to_string(),
        })
    }

    /// Returns the selected bundled version.
    #[must_use]
    pub const fn version(self) -> &'static str {
        self.version
    }

    /// Creates a Tesseract API handle for this runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if Tesseract returns a null API handle.
    pub fn create_engine(self) -> Result<TesseractEngine> {
        let _ = self;
        TesseractEngine::new()
    }
}

/// Returns the embedded bundled Tesseract catalog.
#[must_use]
pub const fn bundled_catalog_toml() -> &'static str {
    include_str!("../../tesseract-sys/bundled/catalog.toml")
}

/// Returns bundled Tesseract versions selected by Cargo features.
#[must_use]
pub fn available_bundled_versions() -> Vec<&'static str> {
    let mut versions = Vec::new();
    if cfg!(feature = "bundled-tesseract-v5-3-4") {
        versions.push("5.3.4");
    }
    versions
}

fn catalog_alias(name: &str) -> String {
    let value: toml::Value =
        toml::from_str(bundled_catalog_toml()).expect("failed to parse bundled Tesseract catalog");
    value
        .get("aliases")
        .and_then(|aliases| aliases.get(name))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("bundled Tesseract catalog alias {name} is required"))
        .to_string()
}

fn version_static(version: &str) -> &'static str {
    available_bundled_versions()
        .into_iter()
        .find(|available| *available == version)
        .unwrap_or_else(|| panic!("bundled Tesseract version {version} is unavailable"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum EngineMode {
    /// Run the legacy engine only.
    TesseractOnly = 0,
    /// Run the LSTM engine only.
    LstmOnly = 1,
    /// Run both legacy and LSTM engines.
    TesseractAndLstm = 2,
    /// Let Tesseract choose the default available engine.
    Default = 3,
}

impl EngineMode {
    /// Converts a raw Tesseract OEM value to an engine mode.
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::TesseractOnly,
            1 => Self::LstmOnly,
            2 => Self::TesseractAndLstm,
            _ => Self::Default,
        }
    }
}

/// Tesseract page segmentation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum PageSegMode {
    /// Orientation and script detection only.
    OsdOnly = 0,
    /// Automatic page segmentation with orientation and script detection.
    AutoOsd = 1,
    /// Automatic page segmentation without orientation/script detection.
    AutoOnly = 2,
    /// Fully automatic page segmentation without orientation/script detection.
    Auto = 3,
    /// Assume a single column of variable-sized text.
    SingleColumn = 4,
    /// Assume a single uniform block of vertically aligned text.
    SingleBlockVertText = 5,
    /// Assume a single uniform block of text.
    SingleBlock = 6,
    /// Treat the image as a single text line.
    SingleLine = 7,
    /// Treat the image as a single word.
    SingleWord = 8,
    /// Treat the image as a single word in a circle.
    CircleWord = 9,
    /// Treat the image as a single character.
    SingleChar = 10,
    /// Sparse text mode.
    SparseText = 11,
    /// Sparse text mode with orientation and script detection.
    SparseTextOsd = 12,
    /// Raw line mode.
    RawLine = 13,
}

impl PageSegMode {
    /// Converts a raw Tesseract PSM value to a page segmentation mode.
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::OsdOnly,
            1 => Self::AutoOsd,
            2 => Self::AutoOnly,
            4 => Self::SingleColumn,
            5 => Self::SingleBlockVertText,
            6 => Self::SingleBlock,
            7 => Self::SingleLine,
            8 => Self::SingleWord,
            9 => Self::CircleWord,
            10 => Self::SingleChar,
            11 => Self::SparseText,
            12 => Self::SparseTextOsd,
            13 => Self::RawLine,
            _ => Self::Auto,
        }
    }
}

/// Initialization options for a Tesseract API session.
#[derive(Clone, Debug)]
pub struct InitOptions {
    /// Optional tessdata directory. Defaults to `resolve_tessdata_dir()`.
    pub datapath: Option<PathBuf>,
    /// Tesseract language code, such as `eng`.
    pub language: String,
    /// Optional engine mode. Defaults to `EngineMode::Default`.
    pub engine_mode: Option<EngineMode>,
}

/// Borrowed raw image buffer passed to Tesseract.
#[derive(Clone, Copy, Debug)]
pub struct ImageView<'a> {
    /// Pixel buffer bytes.
    pub bytes: &'a [u8],
    /// Image width in pixels.
    pub width: i32,
    /// Image height in pixels.
    pub height: i32,
    /// Bytes per pixel.
    pub bytes_per_pixel: i32,
    /// Bytes per image line.
    pub bytes_per_line: i32,
}

/// Per-recognition options.
#[derive(Clone, Debug, Default)]
pub struct RecognitionOptions {
    /// Optional page segmentation mode.
    pub page_seg_mode: Option<PageSegMode>,
    /// Tesseract configuration variables.
    pub variables: Vec<(String, String)>,
}

/// Owned Tesseract API handle.
pub struct TesseractEngine {
    handle: *mut std::ffi::c_void,
}

unsafe impl Send for TesseractEngine {}

impl TesseractEngine {
    /// Creates a new Tesseract API handle.
    ///
    /// # Errors
    ///
    /// Returns an error if Tesseract fails to allocate a handle.
    pub fn new() -> Result<Self> {
        let handle = unsafe { bcode_tesseract_sys::TessBaseAPICreate() };
        if handle.is_null() {
            return Err(Error::CreateHandle);
        }
        Ok(Self { handle })
    }

    /// Returns the linked Tesseract version string.
    #[must_use]
    pub fn version() -> String {
        let version = unsafe { bcode_tesseract_sys::TessVersion() };
        if version.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(version) }
            .to_string_lossy()
            .into_owned()
    }

    /// Initializes this handle for OCR.
    ///
    /// # Errors
    ///
    /// Returns an error if input strings contain NUL bytes or Tesseract init fails.
    pub fn init(&self, options: &InitOptions) -> Result<()> {
        let datapath = options
            .datapath
            .clone()
            .unwrap_or_else(resolve_tessdata_dir);
        let datapath_string = datapath.to_string_lossy().into_owned();
        let datapath_c = CString::new(datapath_string.clone())?;
        let language_c = CString::new(options.language.clone())?;
        let engine_mode = options.engine_mode.unwrap_or(EngineMode::Default) as i32;
        let result = unsafe {
            bcode_tesseract_sys::TessBaseAPIInit2(
                self.handle,
                datapath_c.as_ptr(),
                language_c.as_ptr(),
                engine_mode,
            )
        };
        if result != 0 {
            return Err(Error::Init {
                datapath: datapath_string,
                language: options.language.clone(),
            });
        }
        Ok(())
    }

    /// Runs recognition on an image and returns UTF-8 text.
    ///
    /// # Errors
    ///
    /// Returns an error if Tesseract rejects options, fails recognition, or returns no text.
    pub fn recognize(&self, image: ImageView<'_>, options: &RecognitionOptions) -> Result<String> {
        unsafe {
            bcode_tesseract_sys::TessBaseAPISetImage(
                self.handle,
                image.bytes.as_ptr(),
                image.width,
                image.height,
                image.bytes_per_pixel,
                image.bytes_per_line,
            );
        }

        if let Some(mode) = options.page_seg_mode {
            unsafe { bcode_tesseract_sys::TessBaseAPISetPageSegMode(self.handle, mode as i32) };
        }

        for (name, value) in &options.variables {
            let name_c = CString::new(name.as_str())?;
            let value_c = CString::new(value.as_str())?;
            let result = unsafe {
                bcode_tesseract_sys::TessBaseAPISetVariable(
                    self.handle,
                    name_c.as_ptr(),
                    value_c.as_ptr(),
                )
            };
            if result == 0 {
                return Err(Error::SetVariable { name: name.clone() });
            }
        }

        let result =
            unsafe { bcode_tesseract_sys::TessBaseAPIRecognize(self.handle, ptr::null_mut()) };
        if result != 0 {
            return Err(Error::Recognize);
        }

        let text = unsafe { bcode_tesseract_sys::TessBaseAPIGetUTF8Text(self.handle) };
        if text.is_null() {
            return Err(Error::Text);
        }
        let output = unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned();
        unsafe { bcode_tesseract_sys::TessDeleteText(text) };
        Ok(output)
    }
}

impl Drop for TesseractEngine {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                bcode_tesseract_sys::TessBaseAPIEnd(self.handle);
                bcode_tesseract_sys::TessBaseAPIDelete(self.handle);
            }
        }
    }
}

/// Resolves the default tessdata directory for bundled Tesseract OCR.
#[must_use]
pub fn resolve_tessdata_dir() -> PathBuf {
    if let Ok(prefix) = env::var("TESSDATA_PREFIX") {
        return PathBuf::from(prefix);
    }
    if let Some(prefix) = bcode_tesseract_sys::compiled_tessdata_prefix() {
        return PathBuf::from(prefix);
    }
    if cfg!(target_os = "macos")
        && let Ok(home) = env::var("HOME")
    {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("bcode")
            .join("tessdata");
    }
    if cfg!(target_os = "windows")
        && let Ok(appdata) = env::var("APPDATA")
    {
        return PathBuf::from(appdata).join("bcode").join("tessdata");
    }
    env::var("HOME").map_or_else(
        |_| PathBuf::from("tessdata"),
        |home| PathBuf::from(home).join(".local/share/bcode/tessdata"),
    )
}
