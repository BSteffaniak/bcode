# bcode_tesseract_ocr

Safe Tesseract OCR wrapper with system/linked and bundled runtime support.

## Runtime modes

- `system-tesseract`: link against system Tesseract/Leptonica.
- `bundled-tesseract-default`: build/package the catalog default runtime.
- `bundled-tesseract-latest`: build/package the catalog latest runtime.
- `bundled-tesseract-all`: build/package every catalog runtime.
- `bundled-tesseract-v*`: build/package an explicit catalog runtime.

## High-level API

```rust
use bcode_tesseract_ocr::{ImageView, PageSegMode, RuntimeSelection, TesseractEngine};

let engine = TesseractEngine::builder()
    .runtime(RuntimeSelection::BundledDefault)
    .language("eng")
    .page_seg_mode(PageSegMode::SingleLine)
    .build()?;

let text = engine.recognize(ImageView {
    bytes,
    width,
    height,
    bytes_per_pixel,
    bytes_per_line,
})?;
```

## Runtime lookup

Bundled runtime lookup order:

1. `BCODE_TESSERACT_RUNTIME_ROOT`
2. executable-relative `bcode-runtimes/tesseract`
3. Cargo build-script `OUT_DIR` fallback

## Build/download caches

Bundled builds download source archives and tessdata with SHA-256 verification.

Environment variables:

- `BCODE_TESSERACT_ARTIFACT_CACHE`: override archive/tessdata cache directory.
- `BCODE_TESSERACT_SOURCE_CACHE`: override extracted source cache directory.
- `BCODE_TESSERACT_OFFLINE=1`: require all artifacts to already be cached.
- `BCODE_TESSDATA_PREFIX`: use external tessdata instead of bundled tessdata.

Default cache root is `$XDG_CACHE_HOME/bcode` or `$HOME/.cache/bcode`.

## Packaging and smoke test

```sh
cargo build --package bcode --no-default-features --features bundled-ocr-tesseract-default
cargo xtask package-tesseract-runtimes --binary target/debug/bcode
cargo xtask smoke-test-tesseract --binary target/debug/bcode
```

Packaging writes `bcode-runtimes/tesseract/manifest.json` next to the binary. The smoke test loads bundled runtimes, initializes Tesseract with bundled tessdata, checks invalid-version errors, and OCRs an in-memory `TEST` fixture.
