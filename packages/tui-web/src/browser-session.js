// Concrete browser host. JS owns event/HTTP/platform glue only; Rust owns UI.
// Packaging may import this module and pass initialized wasm/bridge modules.
export async function readBounded(response, limit, signal) {
  if (!response.ok) throw new Error('session request failed');
  const declared = response.headers.get('content-length');
  if (declared !== null && (!/^\d+$/.test(declared) || Number(declared) > limit)) {
    await response.body?.cancel();
    throw new Error('session response too large');
  }
  if (!response.body?.getReader) throw new Error('streaming response required');
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let bytes = 0;
  const chunks = [];
  try {
    while (true) {
      if (signal?.aborted) throw new Error('request aborted');
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > limit) throw new Error('session response too large');
      chunks.push(decoder.decode(value, { stream: true }));
    }
    chunks.push(decoder.decode());
    return chunks.join('');
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
}

// No implicit cross-origin token forwarding. Static embedding requires explicit
// backend origin and exact allowlist consent, HTTPS except loopback development.
export function backendBase(origin, location, allowedOrigins = []) {
  const base = new URL(origin ?? location.origin);
  if (base.username || base.password || base.search || base.hash || base.pathname !== '/') {
    throw new Error('backend must be an origin');
  }
  if (!['http:', 'https:'].includes(base.protocol)) throw new Error('unsupported backend scheme');
  if (base.protocol !== 'https:' && !['localhost', '127.0.0.1', '[::1]'].includes(base.hostname)) {
    throw new Error('insecure backend origin');
  }
  if (base.origin !== location.origin && !allowedOrigins.includes(base.origin)) {
    throw new Error('backend origin requires explicit allowlist');
  }
  return base;
}

/**
 * Mount actual Rust/BMUX UI into an existing Canvas. A packaging entrypoint
 * supplies initialized wasm and BMUX mountInteractiveSurface, not another UI.
 * The backend must permit the static embedding origin through explicit CORS.
 */
export async function mountBrowserSession({
  wasm, mountInteractiveSurface, container, canvas, sessionId, token,
  backendOrigin, allowedBackendOrigins = [], cellWidth = 9, cellHeight = 18,
  fetch: fetchImpl = globalThis.fetch, pollMs = 1000, bindings = {},
  onError = () => {},
}) {
  if (!wasm?.BrowserSession || typeof mountInteractiveSurface !== 'function') {
    throw new Error('initialized WASM and BMUX input bridge required');
  }
  if (!/^[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$/i.test(sessionId) || !token || token.length > 4096) {
    throw new Error('explicit session and launch token required');
  }
  if (!Number.isFinite(cellWidth) || !Number.isFinite(cellHeight) || cellWidth < 1 || cellHeight < 1) {
    throw new Error('invalid cell metrics');
  }
  const window = container.ownerDocument.defaultView;
  const base = backendBase(backendOrigin, window.location, allowedBackendOrigins);
  if (!canvas.id) throw new Error('canvas identity required');
  const controller = new AbortController();
  let disposed = false;
  let timer;
  let queue = Promise.resolve();
  let columns = 0, rows = 0;
  let pendingResize = null;
  let resizeQueued = false;
  let pollQueued = false;
  const rust = new wasm.BrowserSession(sessionId, canvas.id, 0, 0, cellWidth, cellHeight);
  rust.configure_bindings(JSON.stringify(bindings));
  const report = () => { if (!disposed) onError('Browser session operation failed; retry may be required.'); };
  // No Rust mutable borrow survives another callback: every exported async call
  // is awaited in this single queue, including focus cleanup and disposal.
  function enqueue(operation) {
    if (disposed) return Promise.resolve();
    const work = queue.then(() => { if (!disposed) return operation(); });
    queue = work.catch(report);
    return work;
  }
  function endpoint(path) {
    const url = new URL(path, base);
    url.searchParams.set('token', token);
    return url;
  }
  async function request(path, body) {
    if (body !== undefined && new TextEncoder().encode(body).byteLength > 65536) throw new Error('request too large');
    const response = await fetchImpl(endpoint(path), {
      method: body === undefined ? 'GET' : 'POST',
      headers: body === undefined ? { Accept: 'application/json' } : { Accept: 'application/json', 'Content-Type': 'application/json' },
      body, cache: 'no-store', credentials: 'omit', redirect: 'error', referrerPolicy: 'no-referrer', signal: controller.signal,
    });
    return readBounded(response, 2 * 1024 * 1024, controller.signal);
  }
  async function refresh() {
    const json = await request(`/api/session-view/${sessionId}`);
    if (disposed) return;
    // Preserve u64 revisions: never JSON.parse/reencode the observation in JS.
    rust.accept_snapshot(json);
    await rust.present();
  }
  async function submit() {
    const body = rust.retry_submission() ?? rust.prepare_submission(window.crypto.randomUUID());
    const json = await request('/api/session-view/submit-message', body);
    if (disposed) return;
    rust.settle_submission(json);
    await refresh();
  }
  const bridge = mountInteractiveSurface({
    container, canvas,
    mapPoint: ({ x, y }) => x >= 0 && y >= 0 && x < columns * cellWidth && y < rows * cellHeight
      ? { x: x / cellWidth, y: y / cellHeight } : null,
    onInput: event => {
      const json = JSON.stringify(event);
      // Input is finite; no application state or key semantics live in JS.
      void enqueue(async () => {
        const submitRequested = rust.input(json);
        await rust.present();
        const clipboard = rust.take_clipboard();
        if (clipboard !== undefined && clipboard !== null) {
          if (!window.navigator?.clipboard?.writeText) throw new Error('clipboard unavailable');
          await window.navigator.clipboard.writeText(clipboard);
        }
        if (submitRequested) await submit();
      }).catch(() => {});
    },
    onResize: size => {
      pendingResize = size;
      if (resizeQueued) return;
      resizeQueued = true;
      void enqueue(async () => {
        resizeQueued = false;
        const size = pendingResize;
        pendingResize = null;
        const nextColumns = Math.min(1024, Math.max(0, Math.floor(size.width / cellWidth)));
        const nextRows = Math.min(128, Math.max(0, Math.floor(size.height / cellHeight)));
        const dpr = Math.min(4, Math.max(1, size.device_pixel_ratio));
        rust.resize(nextColumns, nextRows, cellWidth * dpr, cellHeight * dpr, `${cellHeight * dpr * 0.8}px monospace`, cellHeight * dpr * 0.8);
        await rust.present();
        columns = nextColumns;
        rows = nextRows;
      }).catch(() => {});
    },
  });
  bridge.setAccessibility({ label: 'Bcode session composer', description: 'Browser BMUX Markdown. Plugin/tool controls, images and history paging unavailable. Enter submits; Shift Enter inserts a newline.' });
  function schedule() {
    if (disposed) return;
    timer = window.setTimeout(() => {
      if (!pollQueued) {
        pollQueued = true;
        void enqueue(refresh).catch(() => {}).finally(() => { pollQueued = false; schedule(); });
      }
    }, Math.max(250, pollMs));
  }
  try {
    await enqueue(refresh);
    schedule();
  } catch (error) {
    disposed = true;
    controller.abort();
    bridge.dispose();
    await queue;
    rust.dispose();
    rust.free();
    throw new Error('Browser session initialization failed');
  }
  return {
    focus: () => { if (!disposed) bridge.focus(); },
    retrySubmission: () => enqueue(submit),
    refresh: () => enqueue(refresh),
    async dispose() {
      if (disposed) return;
      disposed = true;
      controller.abort();
      window.clearTimeout(timer);
      bridge.dispose();
      await queue;
      rust.cancel_pointer();
      rust.dispose();
      rust.free();
    },
  };
}
