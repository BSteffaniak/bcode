# Recap terminal application

The terminal application is enabled by default: build with `cargo build -p bcode_recap_cli`. For a headless-only build, use `--no-default-features`; bare invocation then prints help and the `tui` subcommand is absent. Direct BMUX terminal/image dependencies and the terminal module are compiled only with `tui`. The optional shared BMUX-host compatibility adapter in `bcode_plugin` is separately gated by `bmux-host`, keeping its transitive terminal dependencies out of the headless graph.

With `tui` enabled, Recap's standalone executable supports two presentation paths over the same application operations:

* Bare `recap` opens the terminal application only when standard input and output are terminals.
* `recap tui` explicitly requests the terminal application and rejects unavailable terminal input/output.
* Explicit `create`, `refine`, `export`, `inspect`, and `doctor` commands retain one-shot headless behavior, even when invoked in a terminal. They never enter alternate-screen/raw mode, prompt for missing configuration, or open a browser implicitly.

## Boundaries

The terminal shell lives in the recap CLI package. BMUX owns terminal layout, text measurement, clipping, component interaction geometry, scheduling and presentation. Recap owns semantic content, visual hierarchy and operation policy. Portable recap documents and the processing library do not acquire terminal types.

Shared application operations perform model resolution, tool readiness checks, generation, refinement, storage and HTML export. The TUI calls these operations directly rather than launching its own executable or parsing printed CLI output. Browsing and opening stored documents require no model call.

The terminal application's operation identities fence stale messages. Cancellation remains pending until the actual operation settles; committed revisions remain committed. Source acquisition/media/model work executes outside render calls, and diagnostics must not write over the terminal presentation.

## Presentation

The design is editorial: differentiated canvas and panel surfaces, restrained accent colors, readable warm foregrounds, explicit focus indicators and generous content spacing. Dark, light and reduced-color themes are terminal presentation state, not provider configuration. Narrow layouts collapse navigation rather than squeezing columns. Tiny viewports preserve essential navigation/exit affordances.

HTML remains the full-fidelity portable reader. Terminal presentation adapts document semantics directly; it does not render or parse HTML. Terminal image availability is capability-dependent, with meaningful captions and source references retained when unavailable. Clip playback belongs in the exported HTML rather than a parallel terminal video engine.

## Validation

Validate real terminal entry and restoration in a PTY, plus subprocess behavior with closed stdin and redirected stdout/stderr. Capture home/reader/working/error states at multiple sizes and inspect visible output. Layout checks include long source titles/paths, CJK, emoji, combining marks, focus, resize and clipping. Pipeline tests and offline HTML acceptance remain separate from terminal visual acceptance. A semantic grid capture without styled-cell data is not proof of palette quality.

## Source acquisition

Normal creation has no application-imposed video-size, video-duration, or acquisition
wall-clock cutoff. Cancellation remains available. Download progress/log streams are
drained continuously with bounded diagnostic retention; verbose downloads do not fail
because of their log volume. Structured metadata must still fit the bounded parser.

Advanced callers can explicitly opt into `--max-video-bytes`,
`--max-duration-seconds`, or `--operation-timeout-seconds`; zero (the default) disables
that limit. Source validation, workspace confinement, model context/output constraints,
and sampled-media limits are separate and remain enforced.

### Explicit duration limits

When a known video duration exceeds the current acquisition allowance, creation pauses
before download/model generation and offers **Run anyway for this video** and **Cancel**
(Esc). The prompt shows the actual duration and configured limit, plus time/disk/AI-use
implications. Approval retains the submitted URL, focus, depth, model selection and output
options; it raises only that request's duration allowance. It does not change saved
preferences or declarative configuration. Other limits remain in effect. Unknown duration,
live sources, invalid metadata and permission failures are not duration approvals. Headless
commands stay noninteractive and can set `--max-duration-seconds` explicitly.
