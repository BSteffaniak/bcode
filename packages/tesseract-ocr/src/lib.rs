#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Safe, minimal Tesseract OCR wrapper used by bcode.
//!
//! The wrapper owns API handles, text allocation cleanup, runtime tessdata
//! resolution, and conversion from idiomatic Rust inputs to Tesseract's C API.

use std::env;
use std::ffi::{CStr, CString, NulError};
use std::fs;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
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

    /// Linked Tesseract symbols are unavailable in this build.
    #[error("linked tesseract runtime is unavailable in this build")]
    LinkedRuntimeUnavailable,

    /// A dynamic bundled runtime library could not be loaded.
    #[error("failed to load bundled tesseract runtime '{version}': {message}")]
    LoadBundledRuntime { version: String, message: String },

    /// A bundled runtime path was not emitted by the build script.
    #[error("bundled tesseract runtime path is unavailable in this build")]
    BundledRuntimePathUnavailable,
}

/// Convenient result alias for Tesseract OCR operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A bundled Tesseract runtime selected at compile time.
#[derive(Clone, Debug)]
pub struct TesseractRuntime {
    inner: Arc<TesseractRuntimeInner>,
}

#[derive(Debug)]
struct TesseractRuntimeInner {
    version: &'static str,
    tessdata_dir: PathBuf,
    _library: libloading::Library,
    symbols: TesseractSymbols,
}

#[derive(Clone, Copy, Debug)]
struct TesseractSymbols {
    version: unsafe extern "C" fn() -> *const c_char,
    create: unsafe extern "C" fn() -> *mut c_void,
    delete: unsafe extern "C" fn(*mut c_void),
    end: unsafe extern "C" fn(*mut c_void),
    init2: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    set_image: unsafe extern "C" fn(*mut c_void, *const u8, c_int, c_int, c_int, c_int),
    set_page_seg_mode: unsafe extern "C" fn(*mut c_void, c_int),
    set_variable: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    recognize: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    get_utf8_text: unsafe extern "C" fn(*mut c_void) -> *mut c_char,
    delete_text: unsafe extern "C" fn(*mut c_char),
}

unsafe impl Send for TesseractRuntimeInner {}
unsafe impl Sync for TesseractRuntimeInner {}

impl TesseractRuntime {
    /// Loads the default bundled Tesseract runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if no bundled runtime is available.
    pub fn load_default() -> Result<Self> {
        let default = catalog_alias("default");
        if available_bundled_versions().contains(&default.as_str()) {
            return Self::load_version(&default);
        }
        let latest = catalog_alias("latest");
        if available_bundled_versions().contains(&latest.as_str()) {
            return Self::load_version(&latest);
        }
        let version = available_bundled_versions()
            .first()
            .copied()
            .ok_or(Error::NoBundledRuntime)?;
        Self::load_version(version)
    }

    /// Loads the latest bundled Tesseract runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the latest bundled runtime is not available.
    pub fn load_latest() -> Result<Self> {
        Self::load_version(&catalog_alias("latest"))
    }

    /// Loads a specific bundled Tesseract runtime by catalog version.
    ///
    /// # Errors
    ///
    /// Returns an error if the version was not selected by Cargo features.
    pub fn load_version(version: &str) -> Result<Self> {
        let version = available_bundled_versions()
            .iter()
            .copied()
            .find(|available| *available == version)
            .ok_or_else(|| Error::BundledRuntimeUnavailable {
                version: version.to_string(),
            })?;
        let runtime_dir = bundled_runtime_root()?.join(version);
        let library_path = runtime_dir
            .join("lib")
            .join(dynamic_library_name("tesseract"));
        let tessdata_dir = runtime_dir.join("tessdata");
        let library = unsafe { libloading::Library::new(&library_path) }.map_err(|error| {
            Error::LoadBundledRuntime {
                version: version.to_string(),
                message: runtime_load_diagnostics(&library_path, &error.to_string()),
            }
        })?;
        let symbols = unsafe { load_tesseract_symbols(&library) }.map_err(|error| {
            Error::LoadBundledRuntime {
                version: version.to_string(),
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            inner: Arc::new(TesseractRuntimeInner {
                version,
                tessdata_dir,
                _library: library,
                symbols,
            }),
        })
    }

    /// Returns the selected bundled version.
    #[must_use]
    pub fn version(&self) -> &'static str {
        self.inner.version
    }

    /// Creates a Tesseract API handle for this runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if Tesseract returns a null API handle.
    pub fn create_engine(&self) -> Result<TesseractEngine> {
        TesseractEngine::new_dynamic(Arc::clone(&self.inner))
    }
}

fn bundled_runtime_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("BCODE_TESSERACT_RUNTIME_ROOT") {
        return Ok(PathBuf::from(root));
    }
    if let Some(root) = executable_relative_runtime_root() {
        return Ok(root);
    }
    bcode_tesseract_sys::compiled_bundled_runtime_root()
        .map(PathBuf::from)
        .ok_or(Error::BundledRuntimePathUnavailable)
}

fn executable_relative_runtime_root() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let root = exe_dir.join("bcode-runtimes").join("tesseract");
    root.is_dir().then_some(root)
}

fn runtime_load_diagnostics(library_path: &Path, loader_error: &str) -> String {
    let lib_dir = library_path.parent();
    let entries = lib_dir.and_then(|dir| fs::read_dir(dir).ok()).map_or_else(
        || "<unreadable>".to_string(),
        |entries| {
            let mut names = entries
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>();
            names.sort();
            names.join(", ")
        },
    );
    format!(
        "library: {}; exists: {}; lib dir entries: {}; loader error: {}; hint: {}",
        library_path.display(),
        library_path.exists(),
        entries,
        loader_error,
        platform_loader_hint(library_path)
    )
}

fn platform_loader_hint(library_path: &Path) -> String {
    if cfg!(target_os = "macos") {
        format!("run `otool -L {}`", library_path.display())
    } else if cfg!(target_os = "linux") {
        format!("run `ldd {}`", library_path.display())
    } else {
        "verify the dependent DLLs are next to the runtime library".to_string()
    }
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

unsafe fn load_tesseract_symbols(
    library: &libloading::Library,
) -> std::result::Result<TesseractSymbols, libloading::Error> {
    Ok(TesseractSymbols {
        version: *unsafe { library.get(b"TessVersion\0")? },
        create: *unsafe { library.get(b"TessBaseAPICreate\0")? },
        delete: *unsafe { library.get(b"TessBaseAPIDelete\0")? },
        end: *unsafe { library.get(b"TessBaseAPIEnd\0")? },
        init2: *unsafe { library.get(b"TessBaseAPIInit2\0")? },
        set_image: *unsafe { library.get(b"TessBaseAPISetImage\0")? },
        set_page_seg_mode: *unsafe { library.get(b"TessBaseAPISetPageSegMode\0")? },
        set_variable: *unsafe { library.get(b"TessBaseAPISetVariable\0")? },
        recognize: *unsafe { library.get(b"TessBaseAPIRecognize\0")? },
        get_utf8_text: *unsafe { library.get(b"TessBaseAPIGetUTF8Text\0")? },
        delete_text: *unsafe { library.get(b"TessDeleteText\0")? },
    })
}

/// Returns the embedded bundled Tesseract catalog.
#[must_use]
pub const fn bundled_catalog_toml() -> &'static str {
    include_str!("../../tesseract-sys/bundled/catalog.generated.toml")
}

/// Returns bundled Tesseract versions selected by Cargo features.
#[must_use]
pub fn available_bundled_versions() -> Vec<&'static str> {
    let mut versions = Vec::new();
    if cfg!(feature = "bundled-tesseract-v5-3-4") {
        versions.push("5.3.4");
    }
    if cfg!(feature = "bundled-tesseract-v5-5-1") {
        versions.push("5.5.1");
    }
    versions
}

/// Returns the catalog default bundled Tesseract version.
#[must_use]
pub fn bundled_default_version() -> String {
    catalog_alias("default")
}

/// Returns the catalog latest bundled Tesseract version.
#[must_use]
pub fn bundled_latest_version() -> String {
    catalog_alias("latest")
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
    handle: *mut c_void,
    backend: Backend,
}

#[cfg_attr(
    any(
        feature = "bundled-tesseract",
        feature = "bundled-tesseract-v5-3-4",
        feature = "bundled-tesseract-v5-5-1"
    ),
    allow(dead_code)
)]
enum Backend {
    Linked,
    Dynamic(Arc<TesseractRuntimeInner>),
}

unsafe impl Send for TesseractEngine {}

impl TesseractEngine {
    /// Creates a new Tesseract API handle.
    ///
    /// # Errors
    ///
    /// Returns an error if Tesseract fails to allocate a handle.
    #[allow(clippy::missing_const_for_fn)]
    pub fn new() -> Result<Self> {
        #[cfg(any(
            feature = "bundled-tesseract",
            feature = "bundled-tesseract-v5-3-4",
            feature = "bundled-tesseract-v5-5-1"
        ))]
        {
            Err(Error::LinkedRuntimeUnavailable)
        }
        #[cfg(not(any(
            feature = "bundled-tesseract",
            feature = "bundled-tesseract-v5-3-4",
            feature = "bundled-tesseract-v5-5-1"
        )))]
        {
            let handle = unsafe { bcode_tesseract_sys::TessBaseAPICreate() };
            if handle.is_null() {
                return Err(Error::CreateHandle);
            }
            Ok(Self {
                handle,
                backend: Backend::Linked,
            })
        }
    }

    fn new_dynamic(runtime: Arc<TesseractRuntimeInner>) -> Result<Self> {
        let handle = unsafe { (runtime.symbols.create)() };
        if handle.is_null() {
            return Err(Error::CreateHandle);
        }
        Ok(Self {
            handle,
            backend: Backend::Dynamic(runtime),
        })
    }

    /// Returns the linked Tesseract version string.
    #[must_use]
    pub fn version() -> String {
        linked_version()
    }

    /// Returns the Tesseract version string for this engine backend.
    #[must_use]
    pub fn backend_version(&self) -> String {
        match &self.backend {
            Backend::Linked => linked_version(),
            Backend::Dynamic(runtime) => {
                let version = unsafe { (runtime.symbols.version)() };
                c_string_lossy(version)
            }
        }
    }

    /// Initializes this handle for OCR.
    ///
    /// # Errors
    ///
    /// Returns an error if input strings contain NUL bytes or Tesseract init fails.
    pub fn init(&self, options: &InitOptions) -> Result<()> {
        let datapath = match (&self.backend, &options.datapath) {
            (_, Some(datapath)) => datapath.clone(),
            (Backend::Dynamic(runtime), None) => runtime.tessdata_dir.clone(),
            (Backend::Linked, None) => resolve_tessdata_dir(),
        };
        let datapath_string = datapath.to_string_lossy().into_owned();
        let datapath_c = CString::new(datapath_string.clone())?;
        let language_c = CString::new(options.language.clone())?;
        let engine_mode = options.engine_mode.unwrap_or(EngineMode::Default) as i32;
        let result = unsafe {
            match &self.backend {
                Backend::Linked => linked_init2(
                    self.handle,
                    datapath_c.as_ptr(),
                    language_c.as_ptr(),
                    engine_mode,
                ),
                Backend::Dynamic(runtime) => (runtime.symbols.init2)(
                    self.handle,
                    datapath_c.as_ptr(),
                    language_c.as_ptr(),
                    engine_mode,
                ),
            }
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
            match &self.backend {
                Backend::Linked => linked_set_image(
                    self.handle,
                    image.bytes.as_ptr(),
                    image.width,
                    image.height,
                    image.bytes_per_pixel,
                    image.bytes_per_line,
                ),
                Backend::Dynamic(runtime) => (runtime.symbols.set_image)(
                    self.handle,
                    image.bytes.as_ptr(),
                    image.width,
                    image.height,
                    image.bytes_per_pixel,
                    image.bytes_per_line,
                ),
            }
        }

        if let Some(mode) = options.page_seg_mode {
            unsafe {
                match &self.backend {
                    Backend::Linked => {
                        linked_set_page_seg_mode(self.handle, mode as i32);
                    }
                    Backend::Dynamic(runtime) => {
                        (runtime.symbols.set_page_seg_mode)(self.handle, mode as i32);
                    }
                }
            }
        }

        for (name, value) in &options.variables {
            let name_c = CString::new(name.as_str())?;
            let value_c = CString::new(value.as_str())?;
            let result = unsafe {
                match &self.backend {
                    Backend::Linked => {
                        linked_set_variable(self.handle, name_c.as_ptr(), value_c.as_ptr())
                    }
                    Backend::Dynamic(runtime) => (runtime.symbols.set_variable)(
                        self.handle,
                        name_c.as_ptr(),
                        value_c.as_ptr(),
                    ),
                }
            };
            if result == 0 {
                return Err(Error::SetVariable { name: name.clone() });
            }
        }

        let result = unsafe {
            match &self.backend {
                Backend::Linked => linked_recognize(self.handle, ptr::null_mut()),
                Backend::Dynamic(runtime) => {
                    (runtime.symbols.recognize)(self.handle, ptr::null_mut())
                }
            }
        };
        if result != 0 {
            return Err(Error::Recognize);
        }

        let text = unsafe {
            match &self.backend {
                Backend::Linked => linked_get_utf8_text(self.handle),
                Backend::Dynamic(runtime) => (runtime.symbols.get_utf8_text)(self.handle),
            }
        };
        if text.is_null() {
            return Err(Error::Text);
        }
        let output = unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            match &self.backend {
                Backend::Linked => linked_delete_text(text),
                Backend::Dynamic(runtime) => (runtime.symbols.delete_text)(text),
            }
        };
        Ok(output)
    }
}

impl Drop for TesseractEngine {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                match &self.backend {
                    Backend::Linked => {
                        linked_end(self.handle);
                        linked_delete(self.handle);
                    }
                    Backend::Dynamic(runtime) => {
                        (runtime.symbols.end)(self.handle);
                        (runtime.symbols.delete)(self.handle);
                    }
                }
            }
        }
    }
}

fn c_string_lossy(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_init2(
    handle: *mut c_void,
    datapath: *const c_char,
    language: *const c_char,
    engine_mode: c_int,
) -> c_int {
    unsafe { bcode_tesseract_sys::TessBaseAPIInit2(handle, datapath, language, engine_mode) }
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_init2(
    _handle: *mut c_void,
    _datapath: *const c_char,
    _language: *const c_char,
    _engine_mode: c_int,
) -> c_int {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_set_image(
    handle: *mut c_void,
    bytes: *const u8,
    width: c_int,
    height: c_int,
    bytes_per_pixel: c_int,
    bytes_per_line: c_int,
) {
    unsafe {
        bcode_tesseract_sys::TessBaseAPISetImage(
            handle,
            bytes,
            width,
            height,
            bytes_per_pixel,
            bytes_per_line,
        );
    }
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_set_image(
    _handle: *mut c_void,
    _bytes: *const u8,
    _width: c_int,
    _height: c_int,
    _bytes_per_pixel: c_int,
    _bytes_per_line: c_int,
) {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_set_page_seg_mode(handle: *mut c_void, mode: c_int) {
    unsafe { bcode_tesseract_sys::TessBaseAPISetPageSegMode(handle, mode) };
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_set_page_seg_mode(_handle: *mut c_void, _mode: c_int) {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_set_variable(
    handle: *mut c_void,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    unsafe { bcode_tesseract_sys::TessBaseAPISetVariable(handle, name, value) }
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_set_variable(
    _handle: *mut c_void,
    _name: *const c_char,
    _value: *const c_char,
) -> c_int {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_recognize(handle: *mut c_void, monitor: *mut c_void) -> c_int {
    unsafe { bcode_tesseract_sys::TessBaseAPIRecognize(handle, monitor) }
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_recognize(_handle: *mut c_void, _monitor: *mut c_void) -> c_int {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_get_utf8_text(handle: *mut c_void) -> *mut c_char {
    unsafe { bcode_tesseract_sys::TessBaseAPIGetUTF8Text(handle) }
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_get_utf8_text(_handle: *mut c_void) -> *mut c_char {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_delete_text(text: *mut c_char) {
    unsafe { bcode_tesseract_sys::TessDeleteText(text) };
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_delete_text(_text: *mut c_char) {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_end(handle: *mut c_void) {
    unsafe { bcode_tesseract_sys::TessBaseAPIEnd(handle) };
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_end(_handle: *mut c_void) {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[cfg(not(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
)))]
unsafe fn linked_delete(handle: *mut c_void) {
    unsafe { bcode_tesseract_sys::TessBaseAPIDelete(handle) };
}

#[cfg(any(
    feature = "bundled-tesseract",
    feature = "bundled-tesseract-v5-3-4",
    feature = "bundled-tesseract-v5-5-1"
))]
unsafe fn linked_delete(_handle: *mut c_void) {
    unreachable!("linked Tesseract backend is unavailable in bundled dynamic builds")
}

#[allow(clippy::missing_const_for_fn)]
fn linked_version() -> String {
    #[cfg(not(any(
        feature = "bundled-tesseract",
        feature = "bundled-tesseract-v5-3-4",
        feature = "bundled-tesseract-v5-5-1"
    )))]
    {
        let version = unsafe { bcode_tesseract_sys::TessVersion() };
        return c_string_lossy(version);
    }
    #[cfg(any(
        feature = "bundled-tesseract",
        feature = "bundled-tesseract-v5-3-4",
        feature = "bundled-tesseract-v5-5-1"
    ))]
    String::new()
}

/// Resolves the default tessdata directory for bundled Tesseract OCR.
#[must_use]
pub fn resolve_tessdata_dir() -> PathBuf {
    if let Ok(prefix) = env::var("TESSDATA_PREFIX") {
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
