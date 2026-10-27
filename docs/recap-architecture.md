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

## Acceptance

Deterministic model adapters test pipeline mechanics, not live model quality. Local media tests must exercise actual extraction, composition, storage, and export. Browser acceptance opens `file://` output offline, with JavaScript enabled and disabled, verifies embedded media behavior, and inspects narrow and wide layouts. YouTube acquisition and live model quality require separate real-path evidence; a supplied transcript or precomposed JSON does not establish those capabilities.
