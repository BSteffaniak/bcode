// Browser platform glue only. Rust/BMUX remains the sole UI/compositor authority.
class ControlOutcomeError extends Error {}

export async function readBounded(response, limit, signal) {
  if (!response.ok) {
    void response.body?.cancel().catch(() => {});
    throw new Error('session request failed');
  }
  const declared = response.headers.get('content-length');
  if (declared !== null && (!/^\d+$/.test(declared) || Number(declared) > limit)) {
    void response.body?.cancel().catch(() => {});
    throw new Error('session response too large');
  }
  if (!response.body?.getReader) throw new Error('streaming response required');
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let bytes = 0, reads = 0;
  const chunks = [];
  let rejectAbort;
  const aborted = new Promise((_, reject) => { rejectAbort = reject; });
  const abort = () => {
    rejectAbort(new Error('request aborted'));
    void reader.cancel().catch(() => {});
  };
  signal?.addEventListener('abort', abort, { once: true });
  try {
    if (signal?.aborted) throw new Error('request aborted');
    while (true) {
      const { done, value } = await Promise.race([reader.read(), aborted]);
      if (signal?.aborted) throw new Error('request aborted');
      if (done) break;
      if (++reads > 32768) throw new Error('session response chunk budget exceeded');
      bytes += value.byteLength;
      if (bytes > limit) throw new Error('session response too large');
      chunks.push(decoder.decode(value, { stream: true }));
    }
    chunks.push(decoder.decode());
    return chunks.join('');
  } catch (error) {
    void reader.cancel().catch(() => {});
    throw error;
  } finally {
    signal?.removeEventListener('abort', abort);
    // A non-cooperative transport may still have a pending read after abort.
    try { reader.releaseLock(); } catch { /* cancellation owns the remaining read */ }
  }
}

// No implicit cross-origin token forwarding. Static embedding requires explicit
// backend origin and exact allowlist consent, HTTPS except loopback development.
export function backendBase(origin, location, allowedOrigins = []) {
  const base = new URL(origin ?? location.origin);
  if (base.username || base.password || base.search || base.hash || base.pathname !== '/') {
    throw new Error('backend must be an origin');
  }
  if (!['http:', 'https:'].includes(base.protocol)) throw new Error('invalid backend protocol');
  if (base.protocol === 'http:' && !['localhost', '127.0.0.1', '[::1]'].includes(base.hostname)) {
    throw new Error('insecure backend origin');
  }
  if (base.origin !== location.origin && !allowedOrigins.includes(base.origin)) {
    throw new Error('backend origin requires explicit allowlist');
  }
  return base;
}

/**
 * Mount actual Rust/BMUX UI. recoveryState is caller-owned, in-memory state only;
 * WASM export_recovery/restore_recovery must exclude observations and geometry.
 * No credentials, durable resume claims, storage or alternative UI live here.
 */
export async function mountBrowserSession({
  wasm, mountInteractiveSurface, container, canvas, sessionId, token,
  backendOrigin, allowedBackendOrigins = [], cellWidth = 9, cellHeight = 18,
  fetch: fetchImpl = globalThis.fetch, pollMs = 1000, requestTimeoutMs = 15000,
  bindings = {}, recoveryState = {}, signal, capabilities = {}, onError = () => {}, onBackendResult = () => {},
}) {
  if (!wasm?.BrowserSession || typeof mountInteractiveSurface !== 'function') {
    throw new Error('initialized WASM and BMUX input bridge required');
  }
  if (!/^[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$/i.test(sessionId) || !token || token.length > 4096) {
    throw new Error('explicit session and launch token required');
  }
  if (![cellWidth, cellHeight, pollMs, requestTimeoutMs].every(Number.isFinite)
      || cellWidth < 1 || cellHeight < 1 || requestTimeoutMs < 1 || requestTimeoutMs > 60000) {
    throw new Error('invalid browser session limits');
  }
  if (signal?.aborted) throw new Error('session mount aborted');
  const window = container.ownerDocument.defaultView;
  const base = backendBase(backendOrigin, window.location, allowedBackendOrigins);
  if (!canvas.id) throw new Error('canvas identity required');
  const controller = new AbortController();
  let disposed = false, timer, bridge, rust, disposal, recoveryReady = false;
  let queue = Promise.resolve(), queued = 0;
  let columns = 0, rows = 0, lastSize;
  let painted = false, paintGeneration = 0;
  async function present() {
    painted = false;
    const committed = await rust.present();
    if (!disposed && committed === true) {
      painted = true; paintGeneration++;
      if (bridge && typeof rust.accessibility === 'function') {
        const json = rust.accessibility();
        if (typeof json !== 'string' || new TextEncoder().encode(json).byteLength > 16384) throw new Error('accessibility metadata too large');
        const metadata = JSON.parse(json);
        if (typeof metadata.label !== 'string' || typeof metadata.description !== 'string') throw new Error('invalid accessibility metadata');
        bridge.setAccessibility({ label: metadata.label, description: metadata.description });
      }
    }
  }
  let pendingResize = null, resizeQueued = false, pollQueued = false;
  const report = error => {
    if (!disposed) {
      try { onError(error instanceof ControlOutcomeError
        ? 'Control outcome may be unknown. Refresh pending state before deciding whether to act again; no automatic retry was sent.'
        : 'Browser session operation failed. Draft and unresolved submission retained; retry or remount may be required.'); } catch { /* observer cannot poison the queue */ }
    }
  };
  function saveRecovery() {
    if (typeof rust.export_recovery !== 'function') throw new Error('recovery capability required');
    const value = rust.export_recovery();
    if (typeof value !== 'string' || new TextEncoder().encode(value).byteLength > 256 * 1024) {
      throw new Error('invalid recovery state');
    }
    recoveryState.value = value;
  }
  function createRust() {
    recoveryReady = false;
    rust = new wasm.BrowserSession(sessionId, canvas.id, 0, 0, cellWidth, cellHeight);
    rust.configure_bindings(JSON.stringify(bindings));
    if (recoveryState.value !== undefined) {
      if (typeof rust.restore_recovery !== 'function') throw new Error('recovery capability required');
      rust.restore_recovery(recoveryState.value);
    }
    recoveryReady = true;
  }
  // One bounded queue: no mutable WASM borrow overlaps another exported call.
  function enqueue(operation) {
    if (disposed) return Promise.resolve();
    if (queued >= 64) { report(); return Promise.reject(new Error('browser input queue full')); }
    queued++;
    const work = queue.then(async () => {
      if (!disposed) {
        try { return await operation(); }
        finally { if (!disposed) saveRecovery(); }
      }
    });
    queue = work.catch(report).finally(() => { queued--; });
    return work;
  }
  function endpoint(path) {
    const url = new URL(path, base);
    url.searchParams.set('token', token);
    return url;
  }
  async function request(path, body) {
    if (disposed) throw new Error('session disposed');
    if (body !== undefined && new TextEncoder().encode(body).byteLength > 65536) throw new Error('request too large');
    const requestController = new AbortController();
    const abort = () => requestController.abort();
    controller.signal.addEventListener('abort', abort, { once: true });
    let rejectAbort;
    const aborted = new Promise((_, reject) => { rejectAbort = reject; });
    const onAbort = () => rejectAbort(new Error('session request interrupted; outcome may be unknown'));
    requestController.signal.addEventListener('abort', onAbort, { once: true });
    const deadline = window.setTimeout(abort, requestTimeoutMs);
    try {
      return await Promise.race([(async () => {
        const response = await fetchImpl(endpoint(path), {
          method: body === undefined ? 'GET' : 'POST',
          headers: body === undefined ? { Accept: 'application/json' } : { Accept: 'application/json', 'Content-Type': 'application/json' },
          body, cache: 'no-store', credentials: 'omit', redirect: 'error', referrerPolicy: 'no-referrer', signal: requestController.signal,
        });
        if (requestController.signal.aborted) {
          void response.body?.cancel().catch(() => {});
          throw new Error('request aborted');
        }
        const json = await readBounded(response, 2 * 1024 * 1024, requestController.signal);
        // Inspect only error envelopes; pass original observation/receipt bytes to Rust.
        const envelope = JSON.parse(json);
        if (envelope.error !== undefined) throw new Error('backend rejected session request');
        return json;
      })(), aborted]);
    } finally {
      window.clearTimeout(deadline);
      controller.signal.removeEventListener('abort', abort);
      requestController.signal.removeEventListener('abort', onAbort);
    }
  }
  function resize(size) {
    const { width, height, device_pixel_ratio: ratio } = size;
    if (![width, height, ratio].every(Number.isFinite) || width < 0 || height < 0 || ratio <= 0) throw new Error('invalid surface size');
    columns = Math.min(1024, Math.floor(width / cellWidth));
    rows = Math.min(128, Math.floor(height / cellHeight));
    const dpr = Math.min(4, Math.max(1, ratio));
    rust.resize(columns, rows, cellWidth * dpr, cellHeight * dpr, `${cellHeight * dpr * 0.8}px monospace`, cellHeight * dpr * 0.8);
    lastSize = size;
  }
  async function acceptObservation(json) {
    if (disposed) return;
    try { rust.accept_snapshot(json); }
    catch (error) {
      if (String(error) !== 'server epoch changed; remount required'
          && String(error) !== 'Error: server epoch changed; remount required') throw error;
      // Never silently discard a draft or pending exact retry during backend restart.
      if (typeof rust.export_recovery !== 'function' || typeof rust.restore_recovery !== 'function') throw error;
      saveRecovery();
      rust.cancel_pointer();
      rust.dispose();
      rust.free();
      rust = undefined;
      createRust();
      if (lastSize) resize(lastSize);
      rust.accept_snapshot(json);
    }
    if (!disposed) await present();
  }
  async function refresh() {
    await acceptObservation(await request(`/api/session-view/${sessionId}`));
  }
  function notify(type, json) {
    if (!disposed) {
      try { onBackendResult({ type, json }); } catch { /* observer owns its failures */ }
    }
  }
  async function loadHistory(direction) {
    if (!['older', 'newer', 'tail'].includes(direction) || typeof rust.prepare_history !== 'function') throw new Error('history capability unavailable');
    const body = rust.prepare_history(direction);
    const json = await request('/api/session-view/history', body);
    if (disposed) return;
    await acceptObservation(json);
    notify('history', json);
  }
  async function control(actionJson) {
    if (typeof actionJson !== 'string' || new TextEncoder().encode(actionJson).byteLength > 65536
        || typeof rust.prepare_control !== 'function' || typeof rust.accept_control !== 'function') throw new Error('control capability unavailable');
    const body = rust.prepare_control(actionJson);
    let failure;
    try {
      const json = await request('/api/session-view/control', body);
      if (disposed) return;
      rust.accept_control(actionJson, json);
      notify('control', json);
    } catch (error) {
      failure = error;
      if (!disposed) {
        try { onError('Control outcome may be unknown. Refresh pending state before deciding whether to act again; no automatic retry was sent.'); } catch { /* observer */ }
      }
    }
    // Controls have no durable retry receipt. Even successful local controller
    // input consumes an observation; get a fresh canonical pending state.
    if (!disposed) {
      try { await refresh(); } catch (error) { failure ??= error; }
    }
    if (failure) throw new ControlOutcomeError('Control outcome may be unknown; refresh required', { cause: failure });
  }
  async function dispatchIntents() {
    if (typeof rust.take_intents !== 'function') return;
    const json = rust.take_intents();
    if (typeof json !== 'string' || new TextEncoder().encode(json).byteLength > 65536) throw new Error('invalid host intents');
    const intents = JSON.parse(json);
    if (!Array.isArray(intents) || intents.length > 16) throw new Error('invalid host intents');
    for (const intent of intents) {
      if (disposed) return;
      switch (intent.type) {
        case 'history': await loadHistory(intent.direction); break;
        case 'control':
          if (typeof intent.action_json !== 'string') throw new Error('invalid control intent');
          await control(intent.action_json); break;
        case 'navigate': {
          if (typeof capabilities.navigate !== 'function' || typeof intent.url !== 'string') throw new Error('navigation capability unavailable');
          const url = new URL(intent.url);
          if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password) throw new Error('unsafe navigation');
          await capabilities.navigate(url.href); break;
        }
        case 'clipboard':
          if (typeof capabilities.writeClipboard !== 'function' || typeof intent.text !== 'string') throw new Error('clipboard capability unavailable');
          await capabilities.writeClipboard(intent.text); break;
        default: throw new Error('unsupported host intent');
      }
    }
  }
  async function submit(retryOnly = false) {
    let body = rust.retry_submission();
    if (body == null) {
      if (retryOnly) throw new Error('no unresolved submission');
      body = rust.prepare_submission(window.crypto.randomUUID());
    }
    // Retain the exact operation before transport begins, even on an ambiguous failure.
    saveRecovery();
    const json = await request('/api/session-view/submit-message', body);
    if (disposed) return;
    rust.settle_submission(json);
    saveRecovery();
    await refresh();
  }
  async function dispose() {
    if (disposal) return disposal;
    disposed = true;
    controller.abort();
    signal?.removeEventListener('abort', externalAbort);
    window.clearTimeout(timer);
    bridge?.dispose();
    disposal = (async () => {
      await queue;
      if (rust) {
        try { if (recoveryReady) saveRecovery(); }
        finally {
          try { rust.cancel_pointer(); rust.dispose(); }
          finally { rust.free(); rust = undefined; }
        }
      }
    })();
    return disposal;
  }
  const externalAbort = () => { void dispose().catch(() => {}); };
  function schedule() {
    if (disposed) return;
    timer = window.setTimeout(() => {
      if (pollQueued) return;
      pollQueued = true;
      void enqueue(refresh).catch(() => {}).finally(() => { pollQueued = false; schedule(); });
    }, Math.max(250, pollMs));
  }
  try {
    createRust();
    bridge = mountInteractiveSurface({
      container, canvas,
      mapPoint: ({ x, y }) => !disposed && painted && x >= 0 && y >= 0 && x < columns * cellWidth && y < rows * cellHeight
        ? { x: x / cellWidth, y: y / cellHeight } : null,
      onInput: event => {
        if (disposed) return;
        const json = JSON.stringify(event);
        const inputGeneration = paintGeneration;
        const positional = event.type === 'pointer' || event.type === 'wheel';
        if (new TextEncoder().encode(json).byteLength > 65536) { report(); return; }
        void enqueue(async () => {
          // A queued coordinate belongs only to the output it was mapped against.
          if (positional && (!painted || inputGeneration !== paintGeneration)) {
            rust.cancel_pointer();
            return;
          }
          const submitRequested = rust.input(json);
          await present();
          if (disposed) return;
          const clipboard = rust.take_clipboard();
          if (clipboard != null) {
            if (typeof capabilities.writeClipboard !== 'function') throw new Error('clipboard capability unavailable');
            await capabilities.writeClipboard(clipboard);
          }
          if (!disposed) await dispatchIntents();
          if (!disposed && submitRequested) await submit();
        }).catch(() => {});
      },
      onResize: size => {
        if (disposed) return;
        // The bridge may already have resized the Canvas backing store. Until
        // the queued Rust repaint commits, no old hit region remains visible.
        painted = false;
        pendingResize = size;
        if (resizeQueued) return;
        resizeQueued = true;
        void enqueue(async () => {
          resizeQueued = false;
          const size = pendingResize;
          pendingResize = null;
          resize(size);
          await present();
        }).catch(() => { resizeQueued = false; });
      },
    });
    bridge.setAccessibility({ label: 'Bcode session', description: '' });
    signal?.addEventListener('abort', externalAbort, { once: true });
    if (signal?.aborted) throw new Error('session mount aborted');
    await enqueue(refresh);
    if (disposed) throw new Error('session mount aborted');
    schedule();
  } catch (error) {
    await dispose();
    throw new Error('Browser session initialization failed', { cause: error });
  }
  return {
    focus: () => { if (!disposed) bridge.focus(); },
    retrySubmission: () => enqueue(() => submit(true)),
    refresh: () => enqueue(refresh),
    loadHistory: direction => enqueue(() => loadHistory(direction)),
    control: actionJson => enqueue(() => control(actionJson)),
    dispose,
  };
}
