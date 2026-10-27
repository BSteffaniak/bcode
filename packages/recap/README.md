# Recap

Create a visual, sound-off recap of a video as one self-contained HTML file. The default mode needs no conversation. Optional focus/refinement uses the same underlying document and source evidence.

## Build

```sh
cargo build -p bcode_recap_cli
```

The executable is `target/debug/recap`. The reusable `bcode_recap` library is in this package; the executable's package is `packages/recap/cli` to keep Bcode SDK dependencies out of the library and avoid a bundled-plugin dependency cycle.

Install `ffmpeg` and `ffprobe` for media processing. YouTube input additionally requires `yt-dlp`. Dependencies are checked, never installed implicitly. Sources must be accessible and lawful to process; access restrictions fail explicitly.

## Generate

```sh
recap doctor
recap create --source lesson.mp4 --transcript lesson.vtt \
  --model-config /absolute/path/provider.toml --profile my-profile \
  --id lesson --output lesson.html

recap create --source 'https://www.youtube.com/watch?v=VIDEO_ID_11' \
  --model-config /absolute/path/provider.toml --profile my-profile \
  --id lesson --output lesson.html
```

The provider configuration is loaded explicitly for embedded SDK execution; no running Bcode daemon is required. Model/profile selection is resolved centrally. Provider credentials never enter the HTML. Inference may incur provider charges according to the selected account.

For local files, supply timestamped VTT/SRT captions or configure an explicit transcription command with the CLI's transcription options. `recap create --help` documents supported tool paths, limits, configuration, and output controls. A source without captions/transcription fails rather than inventing spoken content.

## Refine and export

```sh
recap refine lesson --focus 'Focus on the technical details' \
  --model-config /absolute/path/provider.toml --profile my-profile \
  --output lesson-technical.html
recap inspect lesson
recap export lesson --output lesson.html
```

The default document store is `.recap`; use global `--store /absolute/path` to choose another location. Refine creates a revision with optimistic concurrency checks. An export is a snapshot, not a live agent connection. Existing output files are not overwritten unless explicitly allowed by the CLI.

## HTML

CSS, JavaScript, images, and selected silent clips are embedded. Reading works offline and core content remains present without JavaScript. Following source links requires network access. Media budgets can cause explicit degradation notices; no media silently becomes a remote dependency.

## Bcode integration

`plugins/recap-plugin` calls the Rust library directly and contributes create/refine/export/inspect operations. Model execution requires explicit model-host admission under a configured profile; loading the plugin alone does not grant inference authority. Its tools remain subject to normal agent policy and permissions. See `docs/recap-architecture.md` and the plugin documentation for boundaries and configuration.

## Validation

```sh
cargo test -p bcode_recap
scripts/check-recap.sh
```

The acceptance script uses local ffmpeg/ffprobe and deterministic model responses; it does not call a provider or download a source. Browser checks require an existing Playwright/Chromium installation and exercise offline `file://` reading with JavaScript on/off. Those tests prove mechanics, not live model factual quality. Real acquisition, model generation/refinement, and Bcode-hosted execution require separate acceptance runs.
