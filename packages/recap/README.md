# Recap

Create a visual, sound-off recap of a video as one self-contained HTML file. The default mode needs no conversation. Optional focus/refinement uses the same underlying document and source evidence.

## Build

```sh
cargo build -p bcode_recap_cli
```

The executable is `target/debug/recap`. The reusable `bcode_recap` library is in this package; the executable's package is `packages/recap/cli` to keep Bcode SDK dependencies out of the library and avoid a bundled-plugin dependency cycle.

Install `ffmpeg` and `ffprobe` for media processing. YouTube input additionally requires `yt-dlp`. Dependencies are checked, never installed implicitly. Sources must be accessible and lawful to process; access restrictions fail explicitly.

## Terminal application

The TUI is enabled by default. Build normally for terminal support, or disable default features for a lightweight headless-only executable:

```sh
cargo build -p bcode_recap_cli
# Headless only, without terminal rendering/input dependencies:
cargo build -p bcode_recap_cli --no-default-features
# Likewise for installation:
cargo install --path packages/recap/cli --no-default-features
```

Without `tui`, bare `recap` prints help even in a terminal, and the `tui` subcommand is not present. All one-shot commands are available in either build.

```sh
recap
recap tui --model-config /absolute/path/provider.toml --profile my-profile
recap --store /absolute/path/recaps tui --id existing-recap
```

Bare `recap` opens the BMUX terminal application only with terminal stdin/stdout/stderr; otherwise it prints help. Explicit `recap tui` requires a terminal. Existing one-shot subcommands remain headless even in a terminal: no alternate screen, prompts or implicit browser opening.

The TUI has source/model fields, source/transcript options, a bounded local library, stage/elapsed-time progress, a chapter reader, optional focus refinement, HTML export and explicit browser opening. `F2` generates, `F3` toggles chapters/library, `F4` opens focus/source options, `F5` exports, `F6` opens exported HTML, `F8` cycles themes, and `F10`, `Ctrl+C`, or `Ctrl+Q` quits. Tab and mouse navigate controls. Escape requests cancellation while working. Themes are currently per-launch presentation choices; `NO_COLOR` starts monochrome. Native still previews depend on the terminal's advertised BMUX image capabilities; HTML remains the video-playback surface. A theme change does not rewrite model configuration.

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

## Detailed readable editions

Choose **Detailed** in source options or pass `--depth detailed` to `create`.
Brief and Standard remain selective recaps. Detailed processes timestamped source
windows, inventories substantive information, plans chapters, writes them separately,
and audits against source evidence with bounded targeted repair. Chapter count follows
the material rather than an editorial cap. Optional focus changes emphasis, not coverage.

Detailed performs multiple model requests and can take substantially longer and consume
more usage than Standard. Resource allowances remain explicit: CLI callers can set
`--detailed-max-model-calls`, `--detailed-max-source-windows`, and
`--detailed-max-repair-rounds`; library callers configure `EngineOptions::detailed`
and `EngineOptions::inspection`. Exhausted allowances fail rather than quietly producing
a shorter Standard document. Model adapters without typed analysis support fail explicitly.

Adaptive scene/transcript sampling improves visual evidence, but sampled frames do not
prove complete visual coverage or motion understanding. Reading notes preserve inspection
limitations. Audits and evidence references are safeguards, not a guarantee of factual or
semantic completeness. Intermediate analysis remains operation-local; no implicit durable
checkpoint cache is created. Existing documents remain readable without conversion.

## Validation

```sh
cargo test -p bcode_recap
scripts/check-recap.sh
```

The acceptance script uses local ffmpeg/ffprobe and deterministic model responses; it does not call a provider or download a source. Browser checks require an existing Playwright/Chromium installation and exercise offline `file://` reading with JavaScript on/off. Those tests prove mechanics, not live model factual quality. Real acquisition, model generation/refinement, and Bcode-hosted execution require separate acceptance runs.
