# X-ray web inspector

A small React/TypeScript/Vite frontend for one-way X-ray observation. It does not
submit prompts, cancel work, repair sessions, or reconstruct canonical history.

## Run

With a packaged Bcode build:

```sh
bcode xray SESSION_ID
```

Or use `bcode xray` for a bounded session picker. In the TUI, choose **Open X-ray**
from the command palette or use `/xray`. The enabled X-ray command plugin supplies
this action; disabling it does not disable the standalone CLI.

The launcher serves version-matched bundled assets. By default, opening its localhost
URL connects directly: no pairing code, launch ticket or login is required.
Any local visitor/process able to reach that port can observe the selected session
while the launcher is running; use `--require-pairing` when that is not appropriate. Launching does not run npm, download assets, or require the repository as
its working directory. Preserve your normal configuration, profile, and state-root
options. `--deep` explicitly authorizes sensitive capture; it is not the default.

For an isolated synthetic demo while developing Bcode:

```sh
cargo xtask xray-dev
```

This deliberately builds the locked frontend and a test executable, then launches
with isolated configuration/state and no provider requests. Use `--no-open` to
open the printed URL yourself. A packaged executable also supports `bcode xray
--demo` without building anything.

Optional `bcode xray SESSION_ID --require-pairing` uses a short-lived single-use
ticket in the URL fragment when opening the browser automatically. The
frontend removes it before asynchronous startup, redeems it once, and keeps the
resulting credential in memory, not persistent browser storage. The initial
fragment can still be visible to browser history/extensions; keep launch links
private. Manual fallback uses the credential-free printed URL and the one-time
code. Codes expire after 45 seconds and allow eight attempts; restart the launcher
for a fresh code.

The launcher defaults to loopback and serves assets, pairing and the typed X-ray
gateway on one Origin. It grants access for the selected session and lifetime and
revokes its endpoint on exit. Stopping observation never cancels session execution.
Existing custom X-ray host/security policy remains authoritative.

Over SSH, the launcher avoids opening the remote machine's browser and prints a
single-port forwarding command. Deliberately supply your SSH destination, retain
the printed local port and open the printed URL locally. Enter a pairing code only
if `--require-pairing` was enabled.
No SSH destination is guessed. `--local-browser` explicitly opts into opening a
browser on the serving host.

Release packaging builds assets explicitly before embedding them. Source builds
without embedded assets fail with build guidance; they do not install tooling at
launch. `session xray-web SESSION_ID --asset-dir /absolute/dist` remains an explicit
developer override. Unsafe, symlinked, oversized or unbuilt asset trees fail closed.
Production hosting uses restrictive CSP and exact Host/Origin/fetch-metadata checks;
Vite's development server is not a replacement capability host.

## LAN hosting

Loopback remains the default. To expose the inspector deliberately:

```sh
bcode xray SESSION_ID --host 0.0.0.0 --port 8080 --advertise-host 192.168.1.20
```

Open `http://192.168.1.20:8080/` from another device. Substitute the server's actual
LAN IP. `--advertise-host` is required for wildcard binds and accepts a concrete
IP, not a URL or hostname. Binding directly to a LAN IP does not need a separate
advertised address. IPv6 addresses are supported; address families must match.
Port zero (the default) selects a free port, which the launcher prints.

Host/Origin validation accepts only the exact advertised IP and selected port,
not arbitrary request hosts. The daemon's own diagnostic endpoint stays behind the
native gateway; this does not expose daemon IPC or broaden daemon browser grants.
Network listeners do not automatically open a local browser or print SSH forwarding
instructions. Pairing remains opt-in with `--require-pairing`.

**HTTP is unencrypted.** Anyone who can reach the port can read the selected
observation without pairing. Pairing restricts initial access but does not encrypt
traffic. Use a trusted network/firewall or an encrypted tunnel; do not expose this
port to the public Internet. Some browser features (such as direct disk recording)
require a secure context and remain unavailable over plain LAN HTTP.

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
Host/gateway behavior tests live in `bcode_xray_inspector`; CLI picker and routing
tests live in `bcode_cli::xray_launch::tests`. No production provider
requests are needed for these tests.
