# Recap architecture

Recap turns source video into a source-grounded visual document. Default generation is the primary experience; conversational refinement is optional. Reading an exported recap requires neither Bcode nor a model connection.

## Ownership

* `packages/recap` owns portable document contracts, video acquisition, timestamped evidence, media extraction, composition, revision storage, and HTML export. Model execution is injected through a domain-owned interface. This library does not depend on the Bcode application facade.
* `packages/recap/cli` owns the independently usable `recap` executable and its configured embedded SDK model adapter. Keeping this executable in its own package avoids a dependency cycle through Bcode's bundled-plugin registry.
* `plugins/recap-plugin` adapts Bcode tools and commands to direct Rust library calls. It must not invoke the standalone executable. Inference uses the authorized normalized model-host boundary, not private provider or daemon internals.

The source files for the CLI initially live beside the library sources and are compiled only by the CLI package. Renderer-specific HTML policy stays in the exporter; portable contracts contain no browser, terminal, or HyperChad implementation types.

## Document and evidence

A versioned `RecapDocument` retains source identity, transcript segments, timestamped evidence, an owned asset registry, generation intent, and stable semantic sections/blocks. Unknown schema versions reject without guessing or rewriting. Document revisions are separate from schema versions. Optimistic revision preconditions prevent stale refinement from overwriting newer content.

Text remains untrusted plain text, including model output. References and source ranges are validated before publication. Structured model output is not sufficient evidence of factual accuracy: real source comparison remains necessary for product acceptance.

Acquisition and media operations have explicit input, time, output, and duration budgets. External tools receive argument arrays, never interpolated shell scripts. Missing tools and inaccessible sources produce actionable failures, not synthetic successful recaps. Cancellation is propagated to subprocesses and model execution.

## Export

The first renderer emits one HTML snapshot with embedded CSS, JavaScript, raster images, and budgeted silent clips. It has no CDN, remote font, model credential, or live agent dependency. Core content exists as semantic HTML and remains readable without JavaScript. Source links require network only when followed.

HTML generation escapes source/model text and validates URLs. Asset reads are bounded and confined. Export reports media degradation explicitly; omission must not silently introduce remote dependencies. HTML is derived from the document, not authoritative editable session state. Refinement creates a document revision and a new export, rather than adding a server or agent connection to the page.

## Bcode capability integration

The normalized model-host service gains explicit structured multimodal start and read-only admission operations. Legacy text-only start retains its restrictions; callers must select the new operation and unsupported hosts reject rather than treating it as legacy text. Image payloads, metadata, message counts, schemas, and serialized request size are bounded and validated. The bridge does not accept filesystem image paths or provider-private metadata. Provider selection is fixed by administrator admission, not request payloads. Invocation ownership scopes cancellation and terminal cleanup.

Recap operations can involve multiple resources. The portable tool-policy contract represents these as a bounded non-nested compound of existing operation facts. Every child must authorize; a denial takes precedence over an ask/allow, and nested or empty compound operations fail closed. This does not enable a tool, weaken filesystem/web rules, or grant inference. Literal subprocesses use separate `Process` facts and per-spawn admission under the original authorized tool agent; parent-tool enablement does not confer `process.<executable>` authority. Older consumers that do not recognize the added operations reject them through normal unsupported-variant handling.

Explicit command deadlines retain a 30-second client default and permit opt-in up to 900 seconds. The command client and contextual host enforce the same upper bound; neither reports a timeout as proof of rollback. Reading already-exported HTML has no execution deadline or runtime dependency.

## Acquisition and diagnostics

YouTube acquisition requires a current `yt-dlp` and a supported JavaScript runtime/challenge package. Recap enables installed Node, Bun and QuickJS in addition to yt-dlp's default Deno, but never installs tools, downloads remote challenge components, loads browser cookies, or updates package environments. `--ignore-config` keeps acquisition independent of global downloader configuration. Every format fallback retains the caller's size ceiling and 720p height bound; video-only formats are preferred because composition uses stills, silent clips, and captions. Sites may still reject access: HTTP 403 is evidence of rejection, not proof of a particular cause.

`recap doctor` checks tool availability and optional model readiness, not source reachability. `recap doctor --source URL` explicitly downloads bounded source video/captions without inference, then discards its temporary acquisition workspace. It can be combined with `--yt-dlp PATH` to select a newer package-managed downloader. Updating an old Nix environment is a user/package-management action, not something Recap performs silently.

The portable library emits renderer-neutral diagnostic events through an optional observer attached to cancellation. Events distinguish metadata, download, probing, extraction, composition, output validation, storage, and export, with monotonic timings. An actual model executor invocation and a successful revision commit are separate facts; executor invocation does not prove provider acceptance or billing. Cancellation and process admission remain shared when an observer is attached. Subprocess execution remains domain-generic; acquisition classifies bounded failed stderr into allowlisted categories and discards raw output.

The standalone application retains a maximum of 128 events per operation, including explicit truncation and terminal status. Late updates cannot reopen a terminal report. `--diagnostic-report NEW_FILE.json` explicitly exports a version-one JSON report with an opaque operation ID, normalized selected provider/model/profile, tool versions, stage durations, safe acquisition HTTP/exit facts, and commit/inference observations. The destination is checked before work and written atomically without clobbering. Reports are private local files; no automatic logging, background upload, source-content retention, or report-directory growth occurs. A report-write failure preserves the underlying operation failure and commit observation.

Reports exclude prompts, captions, source URLs, signed URLs, frames, credentials, raw executable output, and provider payloads. Failed operations show their phase, diagnostic ID and safe guidance in CLI/TUI; TUI failures have a scrollable details view retrievable with F9 after navigation. Plugin failures include phase and inference/commit observations without changing successful document response contracts. A successful save followed by failed HTML export remains a committed revision with an export failure, not a failed generation or a successful export.

## Comprehensive Detailed generation

`Detailed` is a source-information-preserving readable adaptation, while Brief and Standard retain selective recap behavior. The portable library owns a multi-stage pipeline behind an injected typed analysis executor: window inventories, inspected visual observations, item-accounted chapter planning, chapter composition, source-based auditing, and bounded targeted repair. Final assembly does not summarize chapters again. Chapter count follows source content rather than a fixed editorial cap.

Source windows and information items carry host-validated evidence identities. Extraction and model-declared coverage are not proof of semantic completeness: audits compare drafts to source evidence, and the reader must retain unresolved acquisition or inspection limitations. Sampled frames do not establish complete visual coverage. Focus changes emphasis rather than implicitly restricting Detailed's coverage.

Resource allowances are distinct from editorial coverage. Exhausted allowances and unsupported analysis execution fail explicitly rather than silently falling back to Standard. Intermediate analysis is operation-local; no implicit durable prompt/checkpoint cache is introduced. The existing versioned document shape and unknown-version rejection remain authoritative. Prior revisions remain unchanged until a validated successor commits.

## Acceptance

Deterministic model adapters test pipeline mechanics, not live model quality. Local media tests must exercise actual extraction, composition, storage, and export. Browser acceptance opens `file://` output offline, with JavaScript enabled and disabled, verifies embedded media behavior, and inspects narrow and wide layouts. YouTube acquisition and live model quality require separate real-path evidence; a supplied transcript or precomposed JSON does not establish those capabilities.
