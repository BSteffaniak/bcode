# X-ray web inspector

A small React/TypeScript/Vite frontend for one-way X-ray observation. It does not
submit prompts, cancel work, repair sessions, or reconstruct canonical history.

## Run

Build the assets once:

```sh
cd packages/xray/web
npm ci --ignore-scripts
npm run build
```

From the repository root, using a Bcode executable built from this revision:

```sh
/path/to/bcode session xray-web SESSION_ID --asset-dir "$PWD/packages/xray/web/dist"
```

Preserve the normal configuration, model profile, and state-root options for your
session. `--no-open` leaves browser opening to you. Paste the one-time code printed
by the launcher into the password field. It expires after 45 seconds and permits
eight attempts; start a new launcher for a new code. Credentials are never placed
in URLs or persistent browser storage. Keep the terminal output private.

The launcher binds only loopback, grants the exact inspector Origin for the
selected session and lifetime, and revokes its endpoint on exit. Closing the
launcher does not cancel session execution. Deep capture requires explicit
`--deep`; ordinary observation cannot silently upgrade itself. Existing custom
X-ray host/security policy remains authoritative. For SSH, deliberately forward
both loopback endpoints and preserve their exact Origins; this initial launcher
does not automatically configure forwarded ports or remote exposure.

Assets are an explicit built directory, not fetched from a CDN. Missing, unsafe,
symlinked, oversized, or unbuilt asset trees fail closed. Production hosting uses
restrictive CSP and exact Host/Origin/fetch-metadata checks. Vite's development
server is for frontend development, not a replacement capability host.

## Inspect

* Follow the live window, pause presentation without pausing ingestion, or stop
  observation without stopping execution.
* Filter with fields such as `domain:provider`, `session:ID`, `request:ID`, quoted
  text, and negation. Invalid fields produce errors. No unrestricted regex.
* Select events for structured evidence, explicit correlations, request/attempt
  evidence, and lazily fetched declared payloads. Observed strings are text, never
  executed HTML. Merely resembling a payload reference does not authorize a read.
* Browser retention, windows, worker queues, payload reads and issue lists are
  bounded. Eviction differs from server loss. Search describes its coverage;
  derived groups are not authoritative execution state.
* Open local bundle folders or inspector JSONL exports. Bundles are untrusted,
  version-checked and incrementally scanned. Payload integrity is verified when
  explicitly read. Export window is explicitly incomplete retained evidence,
  without payloads; it cannot recover evicted data or certify session completeness.
* Offline literal scan can search beyond the imported prefix within explicit scan
  bounds. No sensitive payload is fetched implicitly for search.

The recording module also exposes a bounded opt-in disk sink for integrations;
the initial UI does not yet expose a live-to-disk recording control. Reconnecting
starts a fresh observation, not durable or gapless replay. A full waterfall,
server-side search, remote hosting and cross-session aggregation are not supplied.

## Validation

```sh
cd packages/xray/web
npm run build
npm test
npx playwright install chromium  # only if a compatible browser is not installed
npm run test:browser
```

`PLAYWRIGHT_CHROMIUM_EXECUTABLE` may select an explicit existing test browser.
Browser tests exercise real React/worker ingestion and offline import with a
controlled HTTP boundary; they are not evidence of a real daemon/provider run.
Launcher behavior tests live in `bcode_cli::xray_web::tests`. No production provider
requests are needed for these tests.
