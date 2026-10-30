import init, * as wasm from './bcode_tui_web.js';
import { mountInteractiveSurface } from './interactive-surface.js';
import { mountBrowserSession, commitSemanticRegions, createBrowserPlatformPorts } from './browser-session.js';

const status = document.getElementById('status');
const retry = document.getElementById('retry-submission');
const refresh = document.getElementById('refresh-session');
const acknowledge = document.getElementById('acknowledge-control');
const semantics = document.getElementById('semantics');
function setControls(state = {}) {
  retry.hidden = !state.unresolvedSubmission;
  retry.disabled = !host || !state.unresolvedSubmission;
  acknowledge.hidden = !state.controlUncertain;
  acknowledge.disabled = !host || !state.controlUncertain;
  refresh.disabled = !host;
}
function runExplicit(operation) {
  const current = generation, active = host;
  retry.disabled = refresh.disabled = acknowledge.disabled = true;
  void operation().then(() => { if (current === generation && host === active) active?.focus(); }).catch(() => {
    if (current !== generation || host !== active) return;
    status.textContent = 'Operation unresolved. Draft retained. Refresh before another control; retry submission sends its original identity.';
  }).finally(() => { if (current === generation && host === active && host) setControls(lastState); });
}
retry.addEventListener('click', () => { if (host) runExplicit(() => host.retrySubmission()); });
refresh.addEventListener('click', () => { if (host) runExplicit(() => host.refresh()); });
acknowledge.addEventListener('click', () => { if (host) runExplicit(() => host.acknowledgeControl()); });
let host, mounting, lifetime;
let generation = 0;
let lastState = {};
setControls();
// Caller-owned and memory-only: never persist launch credentials or snapshots.
const recoveryState = {};
// Hash launch capabilities are not sent to the static asset server.
const params = new URLSearchParams(location.hash.slice(1));
const query = new URLSearchParams(location.search);
const token = params.get('token') ?? query.get('token');
const sessionId = params.get('session') ?? query.get('session');
const backendOrigin = params.get('backend') ?? undefined;
// Never accept a URL-supplied allowlist that could authorize token exfiltration.
const allowedBackendOrigins = globalThis.bcodeTuiConfig?.allowedBackendOrigins ?? [];
if (token) {
  query.delete('token');
  params.delete('token');
  const search = query.toString(), hash = params.toString();
  history.replaceState(null, '', `${location.pathname}${search ? `?${search}` : ''}${hash ? `#${hash}` : ''}`);
}
const initialized = init({ module_or_path: new URL('./bcode_tui_web_bg.wasm', import.meta.url) });
async function stop() {
  generation++;
  lifetime?.abort();
  const previous = host;
  host = undefined;
  setControls();
  commitSemanticRegions(semantics, []);
  await previous?.dispose();
}
async function start() {
  const current = ++generation;
  const previous = mounting;
  host = undefined;
  setControls();
  lifetime?.abort();
  const controller = new AbortController();
  lifetime = controller;
  try {
    // A restored bfcache page must not create a second retained compositor while
    // the old mount/presentation is settling.
    await previous?.then(next => next.dispose()).catch(() => {});
    await initialized;
    if (current !== generation || controller.signal.aborted) return;
    const platform = createBrowserPlatformPorts({
      root: document.getElementById('platform-dialogs'), signal: controller.signal,
      onError: message => { if (current === generation) status.textContent = message; },
    });
    mounting = mountBrowserSession({
      wasm, mountInteractiveSurface,
      container: document.getElementById('surface'), canvas: document.getElementById('bcode-canvas'),
      sessionId, token, backendOrigin, allowedBackendOrigins, recoveryState, signal: controller.signal,
      capabilities: { ...platform, ...globalThis.bcodeTuiConfig?.capabilities, dispose: () => platform.dispose() },
      onState: state => {
        if (current !== generation) return;
        lastState = state;
        setControls(state);
        if (state.unresolvedSubmission) status.textContent = 'Unresolved submission retained. Retry sends the same identity; no automatic replay.';
        else if (state.controlUncertain) status.textContent = 'Control outcome unknown. Refresh and explicitly acknowledge before another decision.';
        else if (host) status.textContent = 'Browser BMUX session connected';
      },
      onSemantics: regions => { if (current === generation) commitSemanticRegions(semantics, regions); },
      onError: message => { if (current === generation) status.textContent = message; },
    });
    const next = await mounting;
    if (current !== generation || controller.signal.aborted) await next.dispose();
    else {
      host = next;
      status.textContent = 'Browser BMUX session connected';
      setControls(lastState);
      if (lastState.unresolvedSubmission) status.textContent = 'Unresolved submission retained. Review before exact retry.';
      host.focus();
    }
  } catch {
    controller.abort();
    if (current === generation) status.textContent = 'Browser TUI unavailable. Draft recovery may require remount; verify session, launch token and backend access.';
  }
}
window.addEventListener('pagehide', () => { void stop().catch(() => {}); });
window.addEventListener('pageshow', event => { if (event.persisted) void start(); });
await start();
