//! Generated, nonsensitive visual probes. Answers are fixture data, not request metadata.

use base64::Engine as _;
use bcode_model::{ImageContent, ImageMetadata};
use image::{ImageEncoder as _, Rgb, RgbImage};

/// A generated order-sensitive visual fixture and its withheld answer.
pub struct GeneratedImageFixture {
    /// Two PNG images in presentation order.
    pub images: Vec<ImageContent>,
    /// Question that contains neither the selected colors nor their order.
    pub question: String,
    /// Exact expected answer; never include in provider requests or public reports.
    pub expected_answer: String,
}

/// Generate two images, each containing two distinctly colored vertical panels.
///
/// The seed selects one of 1,296 possible ordered four-panel combinations. It is a
/// reproducibility input, not a secret or a cryptographic randomness source. A no-image control
/// remains necessary to identify guesses. Each image is 256 by 128 pixels, without textual clues.
///
/// # Errors
///
/// Returns an error if PNG encoding fails.
pub fn generate_image_fixture(seed: u64) -> Result<GeneratedImageFixture, String> {
    const COLORS: [(&str, [u8; 3]); 6] = [
        ("red", [255, 0, 0]),
        ("green", [0, 180, 0]),
        ("blue", [0, 0, 255]),
        ("yellow", [255, 255, 0]),
        ("black", [0, 0, 0]),
        ("white", [255, 255, 255]),
    ];
    let mut selection = seed;
    let mut answer = Vec::new();
    let mut images = Vec::new();
    for _ in 0..2 {
        let mut pixels = RgbImage::new(256, 128);
        for panel in 0..2 {
            let (name, color) =
                COLORS[usize::try_from(selection % 6).map_err(|_| "invalid fixture color")?];
            selection /= 6;
            answer.push(name);
            for y in 0..128 {
                for x in (panel * 128)..((panel + 1) * 128) {
                    pixels.put_pixel(x, y, Rgb(color));
                }
            }
        }
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(pixels.as_raw(), 256, 128, image::ExtendedColorType::Rgb8)
            .map_err(|_| "image fixture encoding failed")?;
        images.push(ImageContent {
            mime_type: "image/png".to_string(),
            metadata: ImageMetadata {
                width: Some(256),
                height: Some(128),
                byte_len: Some(bytes.len() as u64),
                ..ImageMetadata::default()
            },
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(GeneratedImageFixture {
        images,
        question: "There are two images, each with two colored panels. Name the left then right panel color in the first image, then the left then right panel color in the second image. Reply only with four lowercase color names separated by single spaces.".to_string(),
        expected_answer: answer.join(" "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_pixels_match_withheld_answer_and_order() {
        // Base-six digits: red, green, blue, yellow.
        let fixture = generate_image_fixture(6 + 2 * 36 + 3 * 216).expect("fixture");
        assert_eq!(fixture.expected_answer, "red green blue yellow");
        let expected = [[[255, 0, 0], [0, 180, 0]], [[0, 0, 255], [255, 255, 0]]];
        for (image, colors) in fixture.images.iter().zip(expected) {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&image.data_base64)
                .expect("base64");
            let decoded = image::load_from_memory(&bytes).expect("PNG").to_rgb8();
            assert_eq!(decoded.dimensions(), (256, 128));
            assert_eq!(decoded.get_pixel(64, 64).0, colors[0]);
            assert_eq!(decoded.get_pixel(192, 64).0, colors[1]);
            assert!(image.metadata.source_path.is_none());
        }
        assert!(!fixture.question.contains(&fixture.expected_answer));
        assert_ne!(
            fixture.images,
            generate_image_fixture(0).expect("other fixture").images
        );
        assert_eq!(
            fixture.images,
            generate_image_fixture(726).expect("same fixture").images
        );
    }
}
