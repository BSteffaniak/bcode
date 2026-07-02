#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_tesseract_ocr::{
    ImageView, PageSegMode, RuntimeSelection, TesseractEngine, TesseractRuntime,
    available_bundled_versions,
};

fn main() -> bcode_tesseract_ocr::Result<()> {
    let versions = available_bundled_versions();
    println!(
        "available bundled Tesseract versions: {}",
        versions.join(", ")
    );

    let default_runtime = TesseractRuntime::load_default()?;
    println!(
        "default bundled runtime: {} ({})",
        default_runtime.version(),
        default_runtime.tessdata_dir().display()
    );
    let latest_runtime = TesseractRuntime::load_latest()?;
    println!("latest bundled runtime: {}", latest_runtime.version());

    for version in &versions {
        let runtime = TesseractRuntime::load_version(version)?;
        let engine = runtime.create_engine()?;
        engine.init(&bcode_tesseract_ocr::InitOptions {
            datapath: None,
            language: "eng".to_string(),
            engine_mode: None,
        })?;
        println!("loaded and initialized bundled runtime {version}");
    }

    let invalid = TesseractRuntime::load_version("0.0.0");
    assert!(
        invalid.is_err(),
        "invalid bundled runtime unexpectedly loaded"
    );
    println!("invalid runtime produced expected error");

    let engine = TesseractEngine::builder()
        .runtime(RuntimeSelection::BundledDefault)
        .language("eng")
        .page_seg_mode(PageSegMode::SingleLine)
        .variable("tessedit_char_whitelist", "TEST")
        .build()?;
    let fixture = text_fixture();
    let text = engine.recognize(ImageView {
        bytes: &fixture.bytes,
        width: fixture.width,
        height: fixture.height,
        bytes_per_pixel: 1,
        bytes_per_line: fixture.width,
    })?;
    println!("fixture OCR text: {text:?}");
    if !text.to_ascii_uppercase().contains("TEST") {
        return Err(bcode_tesseract_ocr::Error::UnexpectedText {
            expected: "TEST".to_string(),
            actual: text,
        });
    }
    Ok(())
}

struct FixtureImage {
    bytes: Vec<u8>,
    width: i32,
    height: i32,
}

fn text_fixture() -> FixtureImage {
    const SCALE: usize = 8;
    const GLYPH_WIDTH: usize = 5;
    const GLYPH_HEIGHT: usize = 7;
    const GAP: usize = 2;
    const MARGIN: usize = 12;
    let glyphs = [glyph_t(), glyph_e(), glyph_s(), glyph_t()];
    let width = MARGIN * 2 + glyphs.len() * GLYPH_WIDTH * SCALE + (glyphs.len() - 1) * GAP * SCALE;
    let height = MARGIN * 2 + GLYPH_HEIGHT * SCALE;
    let mut bytes = vec![255_u8; width * height];
    let mut x = MARGIN;
    for glyph in glyphs {
        draw_glyph(&mut bytes, width, x, MARGIN, &glyph, SCALE);
        x += (GLYPH_WIDTH + GAP) * SCALE;
    }
    FixtureImage {
        bytes,
        width: i32::try_from(width).expect("fixture width fits i32"),
        height: i32::try_from(height).expect("fixture height fits i32"),
    }
}

fn draw_glyph(bytes: &mut [u8], width: usize, x: usize, y: usize, glyph: &[&str; 7], scale: usize) {
    for (row, pattern) in glyph.iter().enumerate() {
        for (col, pixel) in pattern.bytes().enumerate() {
            if pixel == b'1' {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let index = (y + row * scale + dy) * width + x + col * scale + dx;
                        bytes[index] = 0;
                    }
                }
            }
        }
    }
}

const fn glyph_t() -> [&'static str; 7] {
    [
        "11111", "00100", "00100", "00100", "00100", "00100", "00100",
    ]
}

const fn glyph_e() -> [&'static str; 7] {
    [
        "11111", "10000", "10000", "11110", "10000", "10000", "11111",
    ]
}

const fn glyph_s() -> [&'static str; 7] {
    [
        "11111", "10000", "10000", "11111", "00001", "00001", "11111",
    ]
}
