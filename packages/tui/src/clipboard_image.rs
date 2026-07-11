//! Clipboard image paste support for the TUI.

use bcode_plugin_sdk::path::display_from_current_dir;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_session_models::SessionId;
use serde::Serialize;
use uuid::Uuid;

const MODEL_MAX_DIMENSION: u32 = 2_000;

/// Errors returned while pasting clipboard images.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardImageError {
    /// The system clipboard could not be opened.
    #[error("clipboard unavailable: {0}")]
    ClipboardUnavailable(String),
    /// The clipboard does not currently contain image data.
    #[error("clipboard does not contain an image")]
    NoImage,
    /// The clipboard image dimensions exceed PNG limits.
    #[error("clipboard image is too large to encode as PNG")]
    ImageTooLarge,
    /// The clipboard image could not be encoded as PNG.
    #[error("failed to encode clipboard image: {0}")]
    Encode(#[from] png::EncodingError),
    /// Artifact metadata could not be serialized.
    #[error("failed to serialize clipboard image metadata: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The image path could not be created, moved, or written.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A draft image path does not have the expected Bcode-managed shape.
    #[error("invalid draft clipboard image path: {0}")]
    InvalidDraftPath(String),
}

/// Files produced for a pasted clipboard image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImageArtifact {
    /// Full-resolution source capture encoded as PNG.
    pub source: PathBuf,
    /// Model-friendly resized PNG inserted into the composer.
    pub model: PathBuf,
    /// Metadata describing the source and resized image.
    pub metadata: PathBuf,
}

#[derive(Debug, Serialize)]
struct ClipboardImageMetadata {
    created_at_ms: u64,
    source: ImageFileMetadata,
    model: ImageFileMetadata,
}

#[derive(Debug, Serialize)]
struct ImageFileMetadata {
    path: String,
    mime_type: &'static str,
    width: u32,
    height: u32,
    byte_len: u64,
}

/// Save the current clipboard image to Bcode-managed artifacts.
///
/// Images pasted before a session exists are stored in draft-session artifacts
/// scoped by the launch working directory. They can later be promoted into the
/// newly-created session before the initial prompt is submitted.
///
/// # Errors
///
/// Returns an error when:
///
/// * the OS clipboard cannot be opened;
/// * the clipboard does not contain an image;
/// * the image cannot be encoded as PNG;
/// * artifact metadata cannot be serialized;
/// * artifact directories or files cannot be written.
pub fn save_clipboard_image(
    session_id: Option<SessionId>,
    launch_working_directory: impl AsRef<Path>,
) -> Result<ClipboardImageArtifact, ClipboardImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))?;
    let image = clipboard
        .get_image()
        .map_err(|_| ClipboardImageError::NoImage)?;
    let state_dir = bcode_config::default_state_dir();
    let image_id = Uuid::new_v4();
    let paths = session_id.map_or_else(
        || draft_clipboard_image_artifact_paths(&state_dir, launch_working_directory, image_id),
        |session_id| clipboard_image_artifact_paths(&state_dir, session_id, image_id),
    );
    save_rgba_image_artifact(paths, image.width, image.height, image.bytes.as_ref())
}

fn save_rgba_image_artifact(
    paths: ClipboardImageArtifact,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<ClipboardImageArtifact, ClipboardImageError> {
    if let Some(parent) = paths.source.parent() {
        fs::create_dir_all(parent)?;
    }
    let source_png = encode_png(width, height, rgba)?;
    fs::write(&paths.source, &source_png)?;

    let model_rgba = resized_model_rgba(width, height, rgba)?;
    let model_png = encode_png(
        usize::try_from(model_rgba.width).map_err(|_| ClipboardImageError::ImageTooLarge)?,
        usize::try_from(model_rgba.height).map_err(|_| ClipboardImageError::ImageTooLarge)?,
        &model_rgba.rgba,
    )?;
    fs::write(&paths.model, &model_png)?;

    let metadata = ClipboardImageMetadata {
        created_at_ms: current_time_ms(),
        source: ImageFileMetadata {
            path: paths.source.to_string_lossy().into_owned(),
            mime_type: "image/png",
            width: u32::try_from(width).map_err(|_| ClipboardImageError::ImageTooLarge)?,
            height: u32::try_from(height).map_err(|_| ClipboardImageError::ImageTooLarge)?,
            byte_len: u64::try_from(source_png.len()).unwrap_or(u64::MAX),
        },
        model: ImageFileMetadata {
            path: paths.model.to_string_lossy().into_owned(),
            mime_type: "image/png",
            width: model_rgba.width,
            height: model_rgba.height,
            byte_len: u64::try_from(model_png.len()).unwrap_or(u64::MAX),
        },
    };
    fs::write(&paths.metadata, serde_json::to_vec_pretty(&metadata)?)?;
    Ok(paths)
}

/// Return artifact paths used for a pasted clipboard image.
#[must_use]
pub fn clipboard_image_artifact_paths(
    state_dir: impl AsRef<Path>,
    session_id: SessionId,
    image_id: Uuid,
) -> ClipboardImageArtifact {
    let root = state_dir
        .as_ref()
        .join("sessions")
        .join(session_id.to_string())
        .join("artifacts")
        .join("clipboard-images")
        .join(image_id.to_string());
    ClipboardImageArtifact {
        source: root.join("source.png"),
        model: root.join("model.png"),
        metadata: root.join("metadata.json"),
    }
}

/// Return artifact paths used for a pasted draft-session clipboard image.
#[must_use]
pub fn draft_clipboard_image_artifact_paths(
    state_dir: impl AsRef<Path>,
    launch_working_directory: impl AsRef<Path>,
    image_id: Uuid,
) -> ClipboardImageArtifact {
    let root =
        draft_clipboard_images_root(state_dir, launch_working_directory).join(image_id.to_string());
    ClipboardImageArtifact {
        source: root.join("source.png"),
        model: root.join("model.png"),
        metadata: root.join("metadata.json"),
    }
}

/// Promote referenced draft clipboard images into session artifacts and rewrite their paths.
///
/// # Errors
///
/// Returns an error when referenced draft artifact files cannot be inspected,
/// copied, or rewritten into session-scoped artifacts.
pub fn promote_draft_clipboard_images(
    message: &str,
    launch_working_directory: impl AsRef<Path>,
    session_id: SessionId,
) -> Result<String, ClipboardImageError> {
    promote_draft_clipboard_images_in_state(
        bcode_config::default_state_dir(),
        message,
        launch_working_directory,
        session_id,
    )
}

fn promote_draft_clipboard_images_in_state(
    state_dir: impl AsRef<Path>,
    message: &str,
    launch_working_directory: impl AsRef<Path>,
    session_id: SessionId,
) -> Result<String, ClipboardImageError> {
    let state_dir = state_dir.as_ref();
    let draft_root = draft_clipboard_images_root(state_dir, launch_working_directory);
    if !draft_root.exists() {
        return Ok(message.to_owned());
    }
    let mut rewritten = message.to_owned();
    for entry in fs::read_dir(draft_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let image_id = entry
            .file_name()
            .to_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                ClipboardImageError::InvalidDraftPath(entry.path().display().to_string())
            })?;
        let draft = ClipboardImageArtifact {
            source: entry.path().join("source.png"),
            model: entry.path().join("model.png"),
            metadata: entry.path().join("metadata.json"),
        };
        let draft_model = draft.model.to_string_lossy();
        if !rewritten.contains(draft_model.as_ref()) {
            continue;
        }
        let session = clipboard_image_artifact_paths(state_dir, session_id, image_id);
        if let Some(parent) = session.source.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&draft.source, &session.source)?;
        fs::copy(&draft.model, &session.model)?;
        fs::copy(&draft.metadata, &session.metadata)?;
        rewritten = rewritten.replace(draft_model.as_ref(), &session.model.to_string_lossy());
    }
    Ok(rewritten)
}

fn draft_clipboard_images_root(
    state_dir: impl AsRef<Path>,
    launch_working_directory: impl AsRef<Path>,
) -> PathBuf {
    state_dir
        .as_ref()
        .join("drafts")
        .join("sessions")
        .join(working_directory_key(launch_working_directory.as_ref()))
        .join("artifacts")
        .join("clipboard-images")
}

fn working_directory_key(path: &Path) -> String {
    let mut key = String::new();
    for byte in path.to_string_lossy().as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut key, "{byte:02x}");
    }
    if key.is_empty() {
        "root".to_owned()
    } else {
        key
    }
}

/// Format composer text inserted for a saved clipboard image.
#[must_use]
pub fn pasted_image_text(path: &Path) -> String {
    format!("{}\n", display_from_current_dir(path))
}

struct ModelRgbaImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn resized_model_rgba(
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<ModelRgbaImage, ClipboardImageError> {
    let width = u32::try_from(width).map_err(|_| ClipboardImageError::ImageTooLarge)?;
    let height = u32::try_from(height).map_err(|_| ClipboardImageError::ImageTooLarge)?;
    if width <= MODEL_MAX_DIMENSION && height <= MODEL_MAX_DIMENSION {
        return Ok(ModelRgbaImage {
            width,
            height,
            rgba: rgba.to_vec(),
        });
    }
    let Some(image) = image::RgbaImage::from_raw(width, height, rgba.to_vec()) else {
        return Err(ClipboardImageError::ImageTooLarge);
    };
    let target_width = scaled_dimension(width, width.max(height));
    let target_height = scaled_dimension(height, width.max(height));
    let resized = image::imageops::resize(
        &image,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    Ok(ModelRgbaImage {
        width: resized.width(),
        height: resized.height(),
        rgba: resized.into_raw(),
    })
}

fn scaled_dimension(dimension: u32, largest_dimension: u32) -> u32 {
    let numerator = u64::from(dimension) * u64::from(MODEL_MAX_DIMENSION);
    let denominator = u64::from(largest_dimension);
    u32::try_from((numerator + (denominator / 2)) / denominator)
        .unwrap_or(MODEL_MAX_DIMENSION)
        .max(1)
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, ClipboardImageError> {
    let width = u32::try_from(width).map_err(|_| ClipboardImageError::ImageTooLarge)?;
    let height = u32::try_from(height).map_err(|_| ClipboardImageError::ImageTooLarge)?;
    let mut output = Cursor::new(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(output.into_inner())
}

fn current_time_ms() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        MODEL_MAX_DIMENSION, clipboard_image_artifact_paths, draft_clipboard_image_artifact_paths,
        pasted_image_text, promote_draft_clipboard_images_in_state, resized_model_rgba,
    };
    use bcode_plugin_sdk::path::display_from_current_dir;
    use bcode_session_models::SessionId;
    use uuid::Uuid;

    #[test]
    fn clipboard_image_artifact_paths_are_session_scoped() {
        let session_id =
            SessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let image_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        let paths = clipboard_image_artifact_paths("/state", session_id, image_id);

        assert_eq!(
            paths.model.to_string_lossy(),
            "/state/sessions/11111111-1111-1111-1111-111111111111/artifacts/clipboard-images/22222222-2222-2222-2222-222222222222/model.png"
        );
        assert_eq!(
            paths.source.to_string_lossy(),
            "/state/sessions/11111111-1111-1111-1111-111111111111/artifacts/clipboard-images/22222222-2222-2222-2222-222222222222/source.png"
        );
        assert_eq!(
            paths.metadata.to_string_lossy(),
            "/state/sessions/11111111-1111-1111-1111-111111111111/artifacts/clipboard-images/22222222-2222-2222-2222-222222222222/metadata.json"
        );
    }

    #[test]
    fn draft_clipboard_image_artifact_paths_are_working_directory_scoped() {
        let image_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        let paths = draft_clipboard_image_artifact_paths("/state", "/tmp/project", image_id);

        assert_eq!(
            paths.model.to_string_lossy(),
            "/state/drafts/sessions/2f746d702f70726f6a656374/artifacts/clipboard-images/22222222-2222-2222-2222-222222222222/model.png"
        );
    }

    #[test]
    fn promote_draft_clipboard_images_copies_referenced_artifacts_and_rewrites_message() {
        let temp = tempfile::tempdir().unwrap();
        let session_id =
            SessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let image_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let draft_paths =
            draft_clipboard_image_artifact_paths(temp.path(), "/tmp/project", image_id);
        std::fs::create_dir_all(draft_paths.source.parent().unwrap()).unwrap();
        std::fs::write(&draft_paths.source, b"source").unwrap();
        std::fs::write(&draft_paths.model, b"model").unwrap();
        std::fs::write(&draft_paths.metadata, b"metadata").unwrap();
        let message = format!("look at {}", display_from_current_dir(&draft_paths.model));

        let rewritten = promote_draft_clipboard_images_in_state(
            temp.path(),
            &message,
            "/tmp/project",
            session_id,
        )
        .unwrap();

        let session_paths = clipboard_image_artifact_paths(temp.path(), session_id, image_id);
        assert_eq!(std::fs::read(&session_paths.source).unwrap(), b"source");
        assert_eq!(std::fs::read(&session_paths.model).unwrap(), b"model");
        assert_eq!(std::fs::read(&session_paths.metadata).unwrap(), b"metadata");
        assert_eq!(
            rewritten,
            format!("look at {}", display_from_current_dir(&session_paths.model))
        );
    }

    #[test]
    fn pasted_image_text_adds_trailing_newline() {
        let text = pasted_image_text(std::path::Path::new("/tmp/bcode/image.png"));

        assert_eq!(text, "/tmp/bcode/image.png\n");
    }

    #[test]
    fn resized_model_rgba_caps_largest_dimension() {
        let width = usize::try_from(MODEL_MAX_DIMENSION + 100).unwrap();
        let height = 10_usize;
        let rgba = vec![255; width * height * 4];

        let resized = resized_model_rgba(width, height, &rgba).unwrap();

        assert_eq!(resized.width, MODEL_MAX_DIMENSION);
        assert!(resized.height < MODEL_MAX_DIMENSION);
    }
}
