// Browser platform glue only. Rust/BMUX remains the sole UI/compositor authority.
class ControlOutcomeError extends Error {}

/** Decode only explicitly admitted canonical image bytes; no URL or host access. */
export async function decodeArtifactImage(bytes, contentType, environment = globalThis) {
  if (!(bytes instanceof Uint8Array) || bytes.byteLength === 0 || bytes.byteLength > 128 * 1024
      || !['image/png', 'image/jpeg', 'image/webp'].includes(contentType)
      || typeof environment.createImageBitmap !== 'function' || typeof environment.OffscreenCanvas !== 'function') throw new Error('image decode unavailable');
  // Decode target bounds prevent oversized retained raster allocations. Browser
  // decoder safety remains the platform's responsibility, not a host resource port.
  const bitmap = await environment.createImageBitmap(new Blob([bytes], { type: contentType }), { resizeWidth: 512, resizeQuality: 'high' });
  try {
    if (!Number.isInteger(bitmap.width) || !Number.isInteger(bitmap.height) || bitmap.width < 1 || bitmap.height < 1
        || bitmap.width > 2048 || bitmap.height > 2048 || bitmap.width * bitmap.height > 1024 * 1024) throw new Error('image dimensions exceed budget');
    const canvas = new environment.OffscreenCanvas(bitmap.width, bitmap.height);
    const context = canvas.getContext('2d', { willReadFrequently: true });
    if (!context) throw new Error('image raster unavailable');
    context.drawImage(bitmap, 0, 0);
    const pixels = context.getImageData(0, 0, bitmap.width, bitmap.height).data;
    return { width: bitmap.width, height: bitmap.height, bytes: new Uint8Array(pixels.buffer, pixels.byteOffset, pixels.byteLength) };
  } finally { bitmap.close(); }
}

/** Browser-owned platform dialogs, never a competing BMUX layout or input authority. */
export function createBrowserPlatformPorts({ root, environment = globalThis, signal, onError = () => {} }) {
  if (!root?.ownerDocument) throw new Error('platform dialog root required');
  const document = root.ownerDocument;
  let disposed = false, generation = 0;
  const fail = () => { if (!disposed) onError('Browser platform operation failed. No control or submission was retried.'); };
  function clear() {
    generation++;
    root.replaceChildren();
  }
  function panel(title) {
    if (disposed) throw new Error('platform ports disposed');
    clear();
    const current = generation;
    const dialog = document.createElement('dialog');
    const heading = document.createElement('h2');
    heading.textContent = title;
    dialog.append(heading);
    const close = document.createElement('button');
    close.type = 'button'; close.textContent = 'Close';
    close.addEventListener('click', clear);
    dialog.addEventListener('cancel', event => { event.preventDefault(); clear(); });
    dialog.append(close);
    root.append(dialog);
    dialog.showModal();
    return { dialog, current, live: () => !disposed && current === generation };
  }
  function safeURL(value) {
    const url = new URL(value);
    if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.href.length > 16384) throw new Error('unsafe navigation');
    return url.href;
  }
  function link(dialog, text, href, download = false) {
    const anchor = document.createElement('a');
    anchor.textContent = text; anchor.href = href;
    anchor.rel = 'noopener noreferrer'; anchor.referrerPolicy = 'no-referrer';
    if (download) anchor.download = 'artifact';
    else anchor.target = '_blank';
    dialog.append(anchor);
    return anchor;
  }
  const ports = {
    async navigate(value) {
      const href = safeURL(value);
      const { dialog } = panel('Open external link');
      // An explicit native link remains usable after queued WASM work has lost
      // transient user activation. Never ask window.open to bypass popup policy.
      link(dialog, href, href);
    },
    async writeClipboard(text) {
      if (disposed || typeof text !== 'string' || new TextEncoder().encode(text).byteLength > 65536) throw new Error('invalid clipboard text');
      const current = generation;
      try {
        if (!environment.navigator?.clipboard?.writeText) throw new Error('clipboard unavailable');
        await environment.navigator.clipboard.writeText(text);
      } catch {
        if (disposed || current !== generation) return;
        const { dialog, live } = panel('Copy selection');
        const value = document.createElement('textarea');
        value.value = text; value.readOnly = true;
        value.setAttribute('aria-label', 'Selected text to copy');
        dialog.append(value);
        value.focus(); value.select();
        const copy = document.createElement('button');
        copy.type = 'button'; copy.textContent = 'Copy';
        copy.addEventListener('click', () => {
          if (!live()) return;
          value.focus(); value.select();
          // A user gesture retry for Clipboard API; manual selection stays
          // available on insecure origins or browsers without clipboard access.
          void Promise.resolve().then(() => environment.navigator?.clipboard?.writeText(text)).catch(fail);
        });
        dialog.append(copy);
      }
    },
    async consumeArtifact({ bytes, contentType, downloadURL, downloadUnavailable, readNext, presentation = 'view', json }) {
      if (!(bytes instanceof Uint8Array) || bytes.byteLength > 128 * 1024 || typeof json !== 'string') throw new Error('invalid artifact content');
      const { dialog, live } = panel('Canonical artifact');
      if (downloadURL) {
        const url = new URL(downloadURL);
        if (url.origin !== environment.location.origin || url.pathname !== '/api/session-view/artifact/download'
            || url.username || url.password || url.hash) throw new Error('unsafe artifact download');
        link(dialog, 'Download complete artifact', url.href, true);
        const caveat = document.createElement('p');
        caveat.textContent = 'Download is limited to finalized artifacts up to 64 MiB. If the artifact changes or authorization expires, the host returns a JSON error instead of artifact bytes.';
        dialog.append(caveat);
      }
      if (downloadUnavailable) {
        const warning = document.createElement('p');
        warning.setAttribute('role', 'status');
        warning.textContent = downloadUnavailable;
        dialog.append(warning);
      }
      const description = document.createElement('p');
      const preview = document.createElement('pre');
      preview.setAttribute('aria-label', 'Artifact range preview');
      dialog.append(description, preview);
      function show(range) {
        if (!live()) return;
        // textContent only: HTML, SVG and scripts never become active documents.
        const metadata = JSON.parse(range.json);
        description.textContent = `Byte range: ${range.bytes.byteLength} bytes. Media type: ${range.contentType ?? 'unknown'}.`;
        preview.textContent = new TextDecoder().decode(range.bytes);
        return metadata;
      }
      show({ bytes, contentType, json });
      if (readNext) {
        const next = document.createElement('button');
        next.type = 'button'; next.textContent = 'Read next range';
        next.addEventListener('click', () => {
          if (!live() || next.disabled) return;
          next.disabled = true;
          void readNext().then(range => {
            if (!live()) return;
            if (range) { show(range); next.disabled = false; }
            else next.textContent = 'End of artifact';
          }).catch(() => { if (live()) { next.disabled = false; fail(); } });
        });
        dialog.append(next);
      }
      if (presentation === 'download') description.textContent += ' Choose Download complete artifact to save the original bytes.';
    },
    dispose() {
      if (disposed) return;
      disposed = true; clear();
      signal?.removeEventListener('abort', ports.dispose);
    },
  };
  signal?.addEventListener('abort', ports.dispose, { once: true });
  if (signal?.aborted) ports.dispose();
  return ports;
}

/** Passive accessibility mirror of committed BMUX regions, never an input authority. */
export function commitSemanticRegions(root, regions) {
  if (!root || !Array.isArray(regions) || regions.length > 256) throw new Error('invalid semantic mirror');
  const ids = new Set();
  const nodes = regions.map(region => {
    if (!region || typeof region.id !== 'string' || region.id.length > 1024 || ids.has(region.id)
        || typeof region.role !== 'string' || region.role.length > 128
        || !region.area || !['x', 'y', 'width', 'height'].every(key => Number.isInteger(region.area[key]) && region.area[key] >= 0)
        || region.area.width === 0 || region.area.height === 0) throw new Error('invalid committed semantic region');
    ids.add(region.id);
    for (const key of ['label', 'description', 'text']) {
      if (region[key] != null && (typeof region[key] !== 'string' || region[key].length > 16384)) throw new Error('invalid semantic text');
    }
    const node = root.ownerDocument.createElement('div');
    node.dataset.regionId = region.id;
    // No synthetic click handlers or guessed hit coordinates. Keyboard input
    // continues through the bridge and its committed modal focus policy.
    node.setAttribute('role', 'group');
    node.setAttribute('aria-roledescription', region.role);
    if (region.label) node.setAttribute('aria-label', region.label);
    if (region.description) node.setAttribute('aria-description', region.description);
    if (region.disabled) node.setAttribute('aria-disabled', 'true');
    if (region.focused) node.setAttribute('aria-current', 'true');
    node.textContent = region.text ?? '';
    return node;
  });
  // Validate every region before replacing the last committed mirror.
  root.replaceChildren(...nodes);
}

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

function artifactPosition(json) {
  // Preserve canonical u64 lexemes without rounding through JS Number. Matching
  // complete strings first prevents numeric-looking text from becoming metadata.
  const exact = JSON.parse(json.replace(/"(?:\\.|[^"\\])*"|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g,
    (token, number) => number === undefined ? token : `"${number}"`));
  const original = JSON.parse(json);
  function integer(key) {
    if (typeof original[key] !== 'number' || !Number.isInteger(original[key]) || original[key] < 0
        || typeof exact[key] !== 'string' || !/^\d{1,20}$/.test(exact[key])) throw new Error('invalid artifact position');
    const value = BigInt(exact[key]);
    if (value > 18446744073709551615n) throw new Error('invalid artifact position');
    return value;
  }
  return { offset: integer('offset'), total: integer('total_bytes'), revision: exact.reference_revision, artifactId: exact.artifact_id };
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
  onState = () => {}, onSemantics = () => {},
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
  let painted = false, paintGeneration = 0, observationGeneration = 0, geometryGeneration = 0;
  let visualWork = null, visualDiscoveryQueued = false, visualBurst = 0;
  function visualNow() { return BigInt(Math.floor(window.performance.now())); }
  function releaseVisual(work) {
    if (!work || work.settled || rust !== work.rust) return;
    try { rust.fail_visual_artifact(work.claim, visualNow()); }
    finally { work.settled = true; }
  }
  // Rust selects references and continuation ranges. JS retains only one opaque
  // canonical request/response in flight; producer bytes never enter JS paint.
  function scheduleVisualArtifact() {
    if (disposed || visualWork || visualDiscoveryQueued || !painted || visualBurst >= 16
        || typeof rust.next_visual_artifact !== 'function') return;
    visualDiscoveryQueued = true;
    void enqueue(() => {
      visualDiscoveryQueued = false;
      if (disposed || visualWork || !painted) return;
      if (typeof rust.accept_visual_artifact_claim !== 'function' || typeof rust.fail_visual_artifact !== 'function') throw new Error('visual artifact port unavailable');
      const json = rust.next_visual_artifact(visualNow());
      if (json == null) return;
      if (typeof json !== 'string' || new TextEncoder().encode(json).byteLength > 128 * 1024) throw new Error('invalid visual artifact discovery');
      const lease = JSON.parse(json);
      if (!lease || typeof lease.claim !== 'string' || lease.claim.length === 0 || lease.claim.length > 128) throw new Error('invalid visual artifact claim');
      const work = { rust, claim: lease.claim, observation: observationGeneration, geometry: geometryGeneration, settled: false };
      if (typeof lease.request_json !== 'string' || new TextEncoder().encode(lease.request_json).byteLength > 65536) {
        releaseVisual(work);
        throw new Error('invalid visual artifact request');
      }
      visualWork = work;
      visualBurst++;
      // Network work is outside the native queue so resize and new canonical
      // observations can invalidate it without waiting for transport completion.
      void request('/api/session-view/artifact', lease.request_json, 512 * 1024).then(response => enqueue(async () => {
        if (disposed || rust !== work.rust || work.observation !== observationGeneration
            || work.geometry !== geometryGeneration) {
          releaseVisual(work);
          return;
        }
        const accepted = rust.accept_visual_artifact_claim(work.claim, response, visualNow());
        work.settled = true;
        if (accepted === true) {
          visualWork = null;
          await present(true);
        }
      })).catch(async error => {
        // Native lease mutation also stays on the serialized queue. Remounted
        // or disposed instances are released before they are freed, never here.
        if (!disposed) await enqueue(() => releaseVisual(work)).catch(() => {});
        report(error);
      }).finally(() => {
        if (visualWork === work) visualWork = null;
        // Rust owns expiry, attempt limits and backoff. A subsequent normal
        // presentation discovers retryable work; failures never spin locally.
      });
    }).catch(() => { visualDiscoveryQueued = false; });
  }
  let controlUncertain = recoveryState.controlUncertain === true;
  function publishState() {
    if (disposed) return;
    try {
      onState({ unresolvedSubmission: rust.retry_submission() != null, controlUncertain });
    } catch { /* observers cannot change application or queue state */ }
  }
  function clearSemantics() {
    try { onSemantics([]); } catch { /* observer */ }
  }
  async function present(visualContinuation = false) {
    if (!visualContinuation) visualBurst = 0;
    painted = false;
    // Keep the last committed metadata observable while the replacement is
    // staged. Input remains fenced by `painted` until output commits.
    try {
      const committed = await rust.present();
      if (disposed) return;
      if (committed !== true) {
        clearSemantics();
        return;
      }
      paintGeneration++;
      let regions = [];
      if (bridge && typeof rust.accessibility === 'function') {
        const json = rust.accessibility();
        if (typeof json !== 'string' || new TextEncoder().encode(json).byteLength > 65536) throw new Error('accessibility metadata too large');
        const metadata = JSON.parse(json);
        if (typeof metadata.label !== 'string' || typeof metadata.description !== 'string') throw new Error('invalid accessibility metadata');
        if (metadata.regions !== undefined) {
          if (!Array.isArray(metadata.regions) || metadata.regions.length > 256) throw new Error('invalid semantic regions');
          regions = metadata.regions;
        }
        bridge.setAccessibility({ label: metadata.label, description: metadata.description });
      }
      painted = true;
      try { onSemantics(regions); } catch { /* observer */ }
      publishState();
      scheduleVisualArtifact();
    } catch (error) {
      painted = false;
      clearSemantics();
      throw error;
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
    publishState();
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
        try {
          const result = await operation();
          if (!disposed) saveRecovery();
          return result;
        } catch (error) {
          try { if (!disposed) saveRecovery(); } catch { /* preserve the original failure */ }
          throw error;
        }
      }
    });
    queue = work.catch(report).finally(() => { queued--; });
    return work;
  }
  async function platformWork(operation) {
    if (disposed) throw new Error('session disposed');
    let reject;
    const interrupted = new Promise((_, fail) => { reject = fail; });
    const abort = () => reject(new Error('browser platform operation interrupted'));
    controller.signal.addEventListener('abort', abort, { once: true });
    const deadline = window.setTimeout(abort, requestTimeoutMs);
    try { return await Promise.race([Promise.resolve().then(operation), interrupted]); }
    finally {
      window.clearTimeout(deadline);
      controller.signal.removeEventListener('abort', abort);
    }
  }
  function endpoint(path) {
    const url = new URL(path, base);
    url.searchParams.set('token', token);
    return url;
  }
  async function request(path, body, responseLimit = 2 * 1024 * 1024) {
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
        const json = await readBounded(response, responseLimit, requestController.signal);
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
    try { if (rust.accept_snapshot(json) !== false) observationGeneration++; }
    catch (error) {
      if (String(error) !== 'server epoch changed; remount required'
          && String(error) !== 'Error: server epoch changed; remount required') throw error;
      // Never silently discard a draft or pending exact retry during backend restart.
      if (typeof rust.export_recovery !== 'function' || typeof rust.restore_recovery !== 'function') throw error;
      saveRecovery();
      releaseVisual(visualWork);
      rust.cancel_pointer();
      rust.dispose();
      rust.free();
      rust = undefined;
      createRust();
      if (lastSize) resize(lastSize);
      rust.accept_snapshot(json);
      observationGeneration++;
    }
    if (!disposed) await present();
  }
  async function refresh() {
    await acceptObservation(await request(`/api/session-view/${sessionId}`));
    publishState();
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
    if (typeof rust.accept_history !== 'function') throw new Error('history acceptance unavailable');
    if (rust.accept_history(json, direction) !== false) observationGeneration++;
    await present();
    notify('history', json);
  }
  async function control(actionJson) {
    if (typeof actionJson !== 'string' || new TextEncoder().encode(actionJson).byteLength > 65536
        || typeof rust.prepare_control !== 'function' || typeof rust.accept_control !== 'function') throw new Error('control capability unavailable');
    if (controlUncertain) throw new ControlOutcomeError('Previous control outcome is unresolved; explicitly acknowledge a refreshed state before acting');
    const body = rust.prepare_control(actionJson);
    let failure;
    try {
      const json = await request('/api/session-view/control', body);
      if (disposed) return;
      rust.accept_control(actionJson, json);
      notify('control', json);
    } catch (error) {
      controlUncertain = true;
      recoveryState.controlUncertain = true;
      publishState();
      failure = error;
      if (!disposed) {
        try { onError('Control outcome may be unknown. Refresh pending state before deciding whether to act again; no automatic retry was sent.'); } catch { /* observer */ }
      }
    }
    // Controls have no durable retry receipt. Even successful local controller
    // input consumes an observation; get a fresh canonical pending state.
    if (!disposed) {
      try {
        await refresh();
        if (!failure && typeof rust.acknowledge_control === 'function') {
          rust.acknowledge_control();
          await present();
        }
      } catch (error) { failure ??= error; }
    }
    if (failure) {
      controlUncertain = true;
      recoveryState.controlUncertain = true;
      publishState();
      throw new ControlOutcomeError('Control outcome may be unknown; refresh required', { cause: failure });
    }
  }
  async function readArtifact(itemId, referenceKey, offset = 0n, length = 128 * 1024) {
    if (typeof rust.prepare_artifact !== 'function' || typeof itemId !== 'string' || typeof referenceKey !== 'string'
        || typeof offset !== 'bigint' || offset < 0n || offset > 18446744073709551615n
        || !Number.isInteger(length) || length < 1 || length > 128 * 1024) throw new Error('invalid artifact request');
    const body = rust.prepare_artifact(itemId, referenceKey, offset, length);
    const json = await request('/api/session-view/artifact', body);
    const range = JSON.parse(json);
    if (range.reference_key !== referenceKey || !Array.isArray(range.bytes) || range.bytes.length > length
        || !range.bytes.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255)
        || (range.content_type != null && (typeof range.content_type !== 'string' || range.content_type.length > 256))) throw new Error('invalid artifact response');
    // Metadata stays encoded for exact u64 handling; only bounded bytes enter a
    // caller-injected consumer. No server-supplied URL/path is opened by JS.
    const position = artifactPosition(json);
    if (position.offset !== offset || position.offset + BigInt(range.bytes.length) > position.total) throw new Error('invalid artifact range');
    return { json, bytes: Uint8Array.from(range.bytes), contentType: range.content_type };
  }
  async function consumeArtifact(intent) {
    if (typeof capabilities.consumeArtifact !== 'function' || ![undefined, 'view', 'download'].includes(intent.presentation)) throw new Error('artifact consumer unavailable');
    const fence = rust.prepare_artifact(intent.item_id, intent.reference_key, 0n, 128 * 1024);
    const current = () => !disposed && fence === rust.prepare_artifact(intent.item_id, intent.reference_key, 0n, 128 * 1024);
    const itemId = intent.item_id, referenceKey = intent.reference_key;
    const first = await readArtifact(itemId, referenceKey);
    if (!current()) return;
    const identity = artifactPosition(first.json);
    let offset = BigInt(first.bytes.length);
    let pending = false;
    const download = endpoint('/api/session-view/artifact/download');
    download.searchParams.set('request', rust.prepare_artifact(itemId, referenceKey, 0n, 128 * 1024));
    const metadata = JSON.parse(first.json);
    const downloadUnavailable = identity.total > 64n * 1024n * 1024n
      ? 'Complete download exceeds the 64 MiB browser host budget. Incremental viewing remains available.'
      : metadata.finalized !== true || metadata.complete === false
        ? 'Complete download requires a finalized, complete artifact. Incremental viewing remains available.' : undefined;
    await platformWork(() => capabilities.consumeArtifact({ itemId, referenceKey, ...first, presentation: intent.presentation,
      downloadURL: downloadUnavailable ? undefined : download.href, downloadUnavailable,
      readNext: () => {
        if (!current() || pending) return Promise.reject(new Error('artifact view expired'));
        if (offset >= identity.total) return Promise.resolve(null);
        pending = true;
        return enqueue(async () => {
          if (!current()) throw new Error('artifact view expired');
          const range = await readArtifact(itemId, referenceKey, offset);
          const currentPosition = artifactPosition(range.json);
          if (!current() || currentPosition.total !== identity.total
              || currentPosition.artifactId !== identity.artifactId || currentPosition.revision !== identity.revision
              || range.contentType !== first.contentType || range.bytes.length === 0) throw new Error('artifact changed');
          offset += BigInt(range.bytes.length);
          return range;
        }).finally(() => { pending = false; });
      },
    }));
  }
  async function showArtifactImage(itemId, referenceKey) {
    if (typeof rust.prepare_image !== 'function' || typeof rust.accept_image !== 'function'
        || !rust.prepare_image(itemId, referenceKey)) throw new Error('image reference unavailable');
    const generation = observationGeneration;
    const geometry = geometryGeneration;
    const owner = rust;
    const current = () => !disposed && rust === owner && generation === observationGeneration && geometry === geometryGeneration;
    const range = await readArtifact(itemId, referenceKey);
    const metadata = JSON.parse(range.json);
    if (metadata.offset !== 0 || metadata.total_bytes !== range.bytes.byteLength || metadata.total_bytes > 128 * 1024) throw new Error('incomplete artifact image');
    if (!current()) return false;
    const decoded = await platformWork(() => decodeArtifactImage(range.bytes, range.contentType));
    if (!current()) return false;
    const accepted = rust.accept_image(decoded.bytes, decoded.width, decoded.height);
    if (accepted) await present();
    return accepted;
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
        case 'unavailable':
          if (typeof intent.message !== 'string' || intent.message.length > 1024) throw new Error('invalid availability message');
          try { onError(intent.message); } catch { /* observer */ }
          break;
        case 'history': await loadHistory(intent.direction); break;
        case 'producer':
        case 'control':
          if (typeof intent.action_json !== 'string') throw new Error('invalid control intent');
          await control(intent.action_json); break;
        case 'navigate': {
          if (typeof capabilities.navigate !== 'function' || typeof intent.url !== 'string') throw new Error('navigation capability unavailable');
          const url = new URL(intent.url);
          if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password) throw new Error('unsafe navigation');
          await platformWork(() => capabilities.navigate(url.href)); break;
        }
        case 'image': await showArtifactImage(intent.item_id, intent.reference_key); break;
        case 'artifact': await consumeArtifact(intent); break;
        case 'clipboard':
          if (typeof capabilities.writeClipboard !== 'function' || typeof intent.text !== 'string') throw new Error('clipboard capability unavailable');
          await platformWork(() => capabilities.writeClipboard(intent.text)); break;
        default: throw new Error('unsupported host intent');
      }
    }
  }
  async function submit(retryOnly = false) {
    let body = rust.retry_submission();
    if (body != null && !retryOnly) throw new Error('unresolved submission requires explicit exact retry');
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
    painted = false;
    clearSemantics();
    const retiringVisualWork = visualWork;
    controller.abort();
    signal?.removeEventListener('abort', externalAbort);
    window.clearTimeout(timer);
    bridge?.dispose();
    try { capabilities.dispose?.(); } catch { /* platform cleanup cannot retain Rust */ }
    disposal = (async () => {
      await queue;
      if (rust) {
        try { releaseVisual(retiringVisualWork); if (recoveryReady) saveRecovery(); }
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
      // Bcode's effective keymap includes modified command keys. The generic
      // bridge defaults to unmodified navigation only; Rust remains the sole
      // action resolver (including explicit unbinds and unsupported actions).
      isCapturedKey: event => typeof event.key === 'string' && event.key.length <= 64 &&
        (event.ctrlKey || event.altKey || event.metaKey ||
          ['Enter', 'Tab', 'Escape', 'Backspace', 'Delete', 'ArrowLeft', 'ArrowRight',
            'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown'].includes(event.key)),
      mapPoint: ({ x, y }) => !disposed && painted && x >= 0 && y >= 0 && x < columns * cellWidth && y < rows * cellHeight
        ? { x: x / cellWidth, y: y / cellHeight } : null,
      onInput: event => {
        if (disposed) return;
        const json = JSON.stringify(event);
        const inputGeneration = paintGeneration;
        const inputObservation = observationGeneration;
        const positional = event.type === 'pointer' || event.type === 'wheel';
        if (new TextEncoder().encode(json).byteLength > 65536) { report(); return; }
        void enqueue(async () => {
          // A queued coordinate belongs only to the output it was mapped against.
          if (inputObservation !== observationGeneration) {
            rust.cancel_pointer();
            return;
          }
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
            await platformWork(() => capabilities.writeClipboard(clipboard));
          }
          if (!disposed) await dispatchIntents();
          if (!disposed && submitRequested) await submit();
        }).catch(() => {});
      },
      onResize: size => {
        if (disposed) return;
        geometryGeneration++;
        // The bridge may already have resized the Canvas backing store. Until
        // the queued Rust repaint commits, no old hit region remains visible.
        painted = false;
        clearSemantics();
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
    try { await dispose(); }
    catch (cleanupError) {
      throw new AggregateError([error, cleanupError], 'Browser initialization and cleanup failed');
    }
    throw new Error('Browser session initialization failed', { cause: error });
  }
  return {
    showArtifactImage: (itemId, referenceKey) => enqueue(() => showArtifactImage(itemId, referenceKey)),
    readArtifact: (itemId, referenceKey, offset, length) => enqueue(() => readArtifact(itemId, referenceKey, offset, length)),
    focus: () => { if (!disposed) bridge.focus(); },
    retrySubmission: () => enqueue(() => submit(true)),
    acknowledgeControl: () => enqueue(async () => {
      // An explicit user gesture acknowledges ambiguity, never replays it.
      await refresh();
      if (typeof rust.acknowledge_control === 'function') rust.acknowledge_control();
      controlUncertain = false;
      recoveryState.controlUncertain = false;
      await present();
      publishState();
    }),
    refresh: () => enqueue(refresh),
    loadHistory: direction => enqueue(() => loadHistory(direction)),
    control: actionJson => enqueue(() => control(actionJson)),
    dispose,
  };
}
