import init, * as wasm from './bcode_tui_web.js';
import { mountInteractiveSurface } from './interactive-surface.js';
import { mountBrowserSession } from './browser-session.js';

const status = document.getElementById('status');
let host, mounting, lifetime;
let generation = 0;
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
  await host?.dispose();
  host = undefined;
}
async function start() {
  const current = ++generation;
  const previous = mounting;
  lifetime?.abort();
  const controller = new AbortController();
  lifetime = controller;
  try {
    // A restored bfcache page must not create a second retained compositor while
    // the old mount/presentation is settling.
    await previous?.then(next => next.dispose()).catch(() => {});
    await initialized;
    if (current !== generation || controller.signal.aborted) return;
    mounting = mountBrowserSession({
      wasm, mountInteractiveSurface,
      container: document.getElementById('surface'), canvas: document.getElementById('bcode-canvas'),
      sessionId, token, backendOrigin, allowedBackendOrigins, recoveryState, signal: controller.signal,
      capabilities: globalThis.bcodeTuiConfig?.capabilities ?? {},
      onError: message => { if (current === generation) status.textContent = message; },
    });
    const next = await mounting;
    if (current !== generation || controller.signal.aborted) await next.dispose();
    else {
      host = next;
      status.textContent = 'Browser BMUX session connected';
      host.focus();
    }
  } catch {
    if (current === generation) status.textContent = 'Browser TUI unavailable. Draft recovery may require remount; verify session, launch token and backend access.';
  }
}
window.addEventListener('pagehide', () => { void stop().catch(() => {}); });
window.addEventListener('pageshow', event => { if (event.persisted) void start(); });
await start();
