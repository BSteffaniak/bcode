import init, * as wasm from './bcode_tui_web.js';
import { mountInteractiveSurface } from './interactive-surface.js';
import { mountBrowserSession } from './browser-session.js';

const status = document.getElementById('status');
let host;
let stopping = false;
// Launch parameters are explicit. Hash parameters avoid sending credentials to a
// static asset server. Query parameters remain supported for same-origin launch.
const params = new URLSearchParams(location.hash.slice(1));
const query = new URLSearchParams(location.search);
const token = params.get('token') ?? query.get('token');
const sessionId = params.get('session') ?? query.get('session');
const backendOrigin = params.get('backend') ?? undefined;
// Cross-origin static embedding requires host-provided configuration, never a
// URL-supplied allowlist that could silently authorize token exfiltration.
const allowedBackendOrigins = globalThis.bcodeTuiConfig?.allowedBackendOrigins ?? [];
// Remove credentials from visible browser history before initialization/fetch.
if (token) {
  query.delete('token');
  params.delete('token');
  const search = query.toString(), hash = params.toString();
  history.replaceState(null, '', `${location.pathname}${search ? `?${search}` : ''}${hash ? `#${hash}` : ''}`);
}
async function stop() {
  stopping = true;
  await host?.dispose();
}
window.addEventListener('pagehide', () => { void stop(); }, { once: true });
try {
  await init({ module_or_path: new URL('./bcode_tui_web_bg.wasm', import.meta.url) });
  if (!stopping) {
    host = await mountBrowserSession({
      wasm, mountInteractiveSurface,
      container: document.getElementById('surface'), canvas: document.getElementById('bcode-canvas'),
      sessionId, token, backendOrigin, allowedBackendOrigins,
      onError: message => { status.textContent = message; },
    });
    if (stopping) await host.dispose();
    else {
      status.textContent = 'BMUX shared Markdown · plugin/tool controls and history paging unavailable';
      host.focus();
    }
  }
} catch {
  status.textContent = 'Browser TUI unavailable. Supply an explicit session and launch token; verify backend access.';
}
