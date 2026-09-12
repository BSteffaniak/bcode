//! Deliberately narrow pixel-based model for generated image-input probes.
use base64::Engine as _;
use bcode_model::{ContentBlock, ImageContent, ModelTurnRequest, ToolResultContent};

pub const MODEL: &str = "fake-vision-panels";

pub fn support(features: &mut bcode_model::ModelFeatureSupport) {
    for feature in [
        bcode_model::MediaInputFeature::UserImage,
        bcode_model::MediaInputFeature::ToolResultImage,
    ] {
        features.media_input.insert(
            feature,
            bcode_model::CapabilitySupport::supported(bcode_model::CapabilitySource::TestContract),
        );
    }
}

pub fn answer(request: &ModelTurnRequest, _text: &str) -> Result<String, String> {
    let text = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == bcode_model::MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        })
        .unwrap_or_default();
    let mut images = Vec::new();
    for block in request.messages.iter().flat_map(|message| &message.content) {
        match block {
            ContentBlock::Image { image } => images.push(image),
            ContentBlock::ToolResult { result } => {
                images.extend(result.content.iter().filter_map(|content| match content {
                    ToolResultContent::Image { image } => Some(image),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    if images.len() > 8 {
        return Err("fake vision accepts at most eight panel images".to_string());
    }
    let mut names = Vec::new();
    for image in images {
        names.extend(panel_colors(image)?);
    }
    if text.contains("Reply only READY") {
        return Ok("READY".to_string());
    }
    if names.is_empty() {
        Ok("UNKNOWN".to_string())
    } else {
        Ok(names.join(" "))
    }
}

fn panel_colors(input: &ImageContent) -> Result<[&'static str; 2], String> {
    if input.mime_type != "image/png" || input.data_base64.len() > 1024 * 1024 {
        return Err("fake vision requires bounded PNG panel fixtures".to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&input.data_base64)
        .map_err(|_| "invalid fake vision base64")?;
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(256);
    limits.max_image_height = Some(128);
    limits.max_alloc = Some(1024 * 1024);
    reader.limits(limits);
    let pixels = reader
        .decode()
        .map_err(|_| "invalid fake vision PNG")?
        .to_rgb8();
    if pixels.dimensions() != (256, 128) {
        return Err("fake vision requires 256x128 panels".to_string());
    }
    let names = [
        color(pixels.get_pixel(64, 64).0)?,
        color(pixels.get_pixel(192, 64).0)?,
    ];
    for (x, _, pixel) in pixels.enumerate_pixels() {
        if color(pixel.0)? != names[usize::from(x >= 128)] {
            return Err("fake vision requires uniform color panels".to_string());
        }
    }
    Ok(names)
}

fn color(rgb: [u8; 3]) -> Result<&'static str, String> {
    match rgb {
        [255, 0, 0] => Ok("red"),
        [0, 180, 0] => Ok("green"),
        [0, 0, 255] => Ok("blue"),
        [255, 255, 0] => Ok("yellow"),
        [0, 0, 0] => Ok("black"),
        [255, 255, 255] => Ok("white"),
        _ => Err("unsupported fake vision panel color".to_string()),
    }
}
