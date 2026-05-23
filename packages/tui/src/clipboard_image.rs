//! Clipboard image paste support for the TUI.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use bcode_session_models::SessionId;
use uuid::Uuid;

/// Errors returned while pasting clipboard images.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardImageError {
    /// No active session is available to scope the pasted image path.
    #[error("no active session")]
    NoActiveSession,
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
    /// The image path could not be created or written.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Save the current clipboard image to a Bcode-scoped temporary file.
///
/// # Errors
///
/// Returns an error when:
///
/// * there is no active session;
/// * the OS clipboard cannot be opened;
/// * the clipboard does not contain an image;
/// * the image cannot be encoded as PNG;
/// * the output directory or image file cannot be written.
pub fn save_clipboard_image(session_id: Option<SessionId>) -> Result<PathBuf, ClipboardImageError> {
    let session_id = session_id.ok_or(ClipboardImageError::NoActiveSession)?;
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))?;
    let image = clipboard
        .get_image()
        .map_err(|_| ClipboardImageError::NoImage)?;
    let path = clipboard_image_path(std::env::temp_dir(), session_id, Uuid::new_v4());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &path,
        encode_png(image.width, image.height, image.bytes.as_ref())?,
    )?;
    Ok(path)
}

/// Return the temporary path used for a pasted clipboard image.
#[must_use]
pub fn clipboard_image_path(
    temp_dir: impl AsRef<Path>,
    session_id: SessionId,
    image_id: Uuid,
) -> PathBuf {
    temp_dir
        .as_ref()
        .join("bcode")
        .join("clipboard-images")
        .join(session_id.to_string())
        .join(format!("bcode-clipboard-{image_id}.png"))
}

/// Format composer text inserted for a saved clipboard image.
#[must_use]
pub fn pasted_image_text(path: &Path) -> String {
    format!("{}\n", path.display())
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

#[cfg(test)]
mod tests {
    use super::{clipboard_image_path, pasted_image_text};
    use bcode_session_models::SessionId;
    use uuid::Uuid;

    #[test]
    fn clipboard_image_path_is_session_scoped_png() {
        let session_id =
            SessionId(Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let image_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        let path = clipboard_image_path("/tmp", session_id, image_id);

        assert_eq!(
            path.to_string_lossy(),
            "/tmp/bcode/clipboard-images/11111111-1111-1111-1111-111111111111/bcode-clipboard-22222222-2222-2222-2222-222222222222.png"
        );
    }

    #[test]
    fn pasted_image_text_adds_trailing_newline() {
        let text = pasted_image_text(std::path::Path::new("/tmp/bcode/image.png"));

        assert_eq!(text, "/tmp/bcode/image.png\n");
    }
}
