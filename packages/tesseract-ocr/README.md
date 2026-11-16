# bcode_tesseract_ocr

Safe Tesseract OCR wrapper with system/linked and bundled runtime support.

## Runtime modes

- `system-tesseract`: link against system Tesseract/Leptonica.
- `bundled-tesseract-default`: bundle the catalog default version.
- `bundled-tesseract-latest`: bundle the catalog latest version.
- `bundled-tesseract-v*`: bundle an explicit catalog version.
- `bundled-tesseract-dynamic`: ship bundled versions as loadable runtime trees.
- `bundled-tesseract-all`: bundle every catalog version (implies `bundled-tesseract-dynamic`).

## Bundled linkage

Bundled builds compile Leptonica and Tesseract from checksum-verified catalog sources.

Static linkage is the default. Exactly one version is linked into the binary and the catalog
tessdata is embedded with it, so OCR works with no files beside the executable. A linked engine
without an explicit datapath initializes an embedded language from memory; other languages and
`+`-joined combinations read `<language>.traineddata` from `TESSDATA_PREFIX` or the platform
tessdata directory.

`bundled-tesseract-dynamic` instead builds each selected version as a shared library with its own
tessdata directory and loads it at runtime. Selecting more than one version requires dynamic
linkage, and the build fails otherwise.

`BCODE_TESSERACT_LINK_MODE` overrides the feature-derived mode with `none`, `system`, `vendored`,
`bundled` (feature-derived linkage), `bundled-static`, or `bundled-dynamic`.

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

Dynamic bundled runtime lookup order:

1. `BCODE_TESSERACT_RUNTIME_ROOT`
2. `bcode-runtimes/tesseract` in the artifact's resources: beside the executable, or in the
   daemon's pinned resource snapshot (see `bcode_plugin_sdk::artifact_resources`)

Runtimes are never loaded from the Cargo build directory, so a dynamic build must be packaged
(`cargo xtask package-tesseract-runtimes`) before it can OCR.

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
cargo run --package bcode_tesseract_ocr --bin tesseract-smoke --features bundled-tesseract-default
```

Static builds need no packaging step. For dynamic builds:

```sh
cargo build --package bcode --no-default-features --features bundled-ocr-tesseract-default,bundled-ocr-tesseract-dynamic
cargo xtask package-tesseract-runtimes --binary target/debug/bcode
cargo xtask smoke-test-tesseract --binary target/debug/bcode
```

Packaging writes `bcode-runtimes/tesseract/manifest.json` next to the binary. The smoke test loads bundled runtimes, initializes Tesseract with bundled tessdata, checks invalid-version errors, and OCRs an in-memory `TEST` fixture. Without a runtime tree beside the binary, `smoke-test-tesseract` instead OCRs a fixture through the binary's statically bundled OCR plugin.
