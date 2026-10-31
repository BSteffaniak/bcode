# Recap terminal application

The terminal application is opt-in: build with `cargo build -p bcode_recap_cli --features tui`. Default features remain empty; a default build includes only one-shot commands and bare invocation prints help. Direct BMUX terminal/image dependencies and the terminal module are compiled only with `tui`. The optional shared BMUX-host compatibility adapter in `bcode_plugin` is separately gated by `bmux-host`, keeping its transitive terminal dependencies out of the headless graph.

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
