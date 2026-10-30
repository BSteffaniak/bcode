import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
// Source module has no Node/package dependencies; packaging selects its module URL.
const source = await readFile(new URL('../src/browser-session.js', import.meta.url), 'utf8');
const { readBounded, backendBase, mountBrowserSession, commitSemanticRegions, decodeArtifactImage } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

function response(chunks, headers = {}) {
  let cancelled = false;
  return {
    ok: true, headers: new Headers(headers),
    body: { cancel: async () => { cancelled = true; }, getReader: () => ({
      async read() { return chunks.length ? { value: chunks.shift(), done: false } : { done: true }; },
      async cancel() { cancelled = true; }, releaseLock() {},
    }) },
    get cancelled() { return cancelled; },
  };
}
const jsonResponse = value => response([new TextEncoder().encode(JSON.stringify(value))]);
const tick = () => new Promise(resolve => setImmediate(resolve));
const SESSION = '00000000-0000-4000-8000-000000000001';
const ID = '00000000-0000-4000-8000-000000000002';

// Host contract double only: native semantic validation and real Canvas/browser
// behavior require their owning native tests and packaged browser smoke harness.
async function fixture({ fetch, recoveryState = {}, timeout = 1000, fault, signal, capabilities = {}, intents = [] } = {}) {
  const calls = [], errors = [], results = [], accessibility = [], instances = [], bodies = [], states = [], semantics = [];
  let callbacks;
  class BrowserSession {
    constructor() { this.draft = ''; this.pending = null; this.epoch = null; this.active = false; this.freed = false; instances.push(this); }
    check() { assert.equal(this.active, false); assert.equal(this.freed, false); }
    configure_bindings() { this.check(); if (fault === 'configure') throw new Error('configure failed'); }
    take_clipboard() { this.check(); return null; }
    resize() { this.check(); calls.push('resize'); }
    accept_snapshot(json) {
      this.check(); const { epoch } = JSON.parse(json);
      if (this.epoch !== null && this.epoch !== epoch) throw 'server epoch changed; remount required';
      this.epoch = epoch; calls.push(`accept:${epoch}`);
    }
    prepare_image() { this.check(); return true; }
    accept_image(bytes, width, height) { this.check(); calls.push('image'); return bytes.length === width * height * 4; }
    prepare_artifact(item, reference, offset, length) { this.check(); return `{"session_id":"${SESSION}","item_id":${JSON.stringify(item)},"reference_key":${JSON.stringify(reference)},"offset":${offset},"length":${length}}`; }
    accept_history(json, direction) { this.check(); calls.push(`history:${direction}`); this.accept_snapshot(json); }
    input(json) { this.check(); const event = JSON.parse(json); calls.push(`input:${event.type ?? 'test'}`); if (event.text) this.draft = event.text; return event.submit === true; }
    async present() { this.check(); this.active = true; await tick(); this.active = false; calls.push('present'); return fault !== 'uncommitted'; }
    retry_submission() { this.check(); return this.pending; }
    prepare_submission(identity) { this.check(); this.pending = JSON.stringify({ session_id: SESSION, submission_id: identity, text: this.draft, placement: 'steering' }); return this.pending; }
    settle_submission(json) {
      this.check(); const receipt = JSON.parse(json);
      if (receipt.submission_id !== JSON.parse(this.pending).submission_id) throw new Error('unmatched receipt');
      if (this.draft === JSON.parse(this.pending).text) this.draft = '';
      this.pending = null; calls.push('settled');
    }
    take_intents() { this.check(); return JSON.stringify(intents.splice(0, 16)); }
    accessibility() { this.check(); return JSON.stringify({ label: 'Committed session', description: 'Load older history. New messages below' }); }
    prepare_history(direction) {
      this.check();
      return `{"session_id":"${SESSION}","epoch":"${ID}","revision":18446744073709551615,"direction":"${direction}"}`;
    }
    prepare_control(json) {
      this.check(); const action = JSON.parse(json);
      if (!['permission', 'permission_batch', 'interaction'].includes(action.type)) throw new Error('invalid browser control');
      return `{"session_id":"${SESSION}","epoch":"${ID}","revision":18446744073709551615,"action":${json}}`;
    }
    accept_control(actionJson, json) {
      this.check(); const action = JSON.parse(actionJson); const value = JSON.parse(json);
      const allowed = { permission: ['permission_resolved'], permission_batch: ['permission_batch_resolved'], interaction: ['none', 'interaction_resolved'] };
      if (value.schema_version !== 1 || !allowed[action.type]?.includes(value.outcome?.type)) throw new Error('invalid control response');
      calls.push('control-accepted');
    }
    export_recovery() {
      this.check();
      // Same envelope shape as SessionSurface/WASM; editor buffer is owned by Rust.
      return JSON.stringify({ schema_version: 1, editor: { text: this.draft }, submission: JSON.stringify({ schema_version: 1, session: SESSION, pending: this.pending === null ? null : JSON.parse(this.pending) }) });
    }
    restore_recovery(json) {
      this.check(); const state = JSON.parse(json); assert.equal(state.schema_version, 1);
      this.draft = state.editor.text;
      const submission = JSON.parse(state.submission); assert.equal(submission.session, SESSION);
      this.pending = submission.pending === null ? null : JSON.stringify(submission.pending);
      calls.push('restored');
    }
    cancel_pointer() { this.check(); calls.push('cancel'); }
    dispose() { this.check(); calls.push('dispose'); }
    free() { this.check(); this.freed = true; calls.push('free'); }
  }
  const window = { location: { origin: 'https://ui.example' }, crypto: { randomUUID: () => ID }, setTimeout, clearTimeout };
  const canvas = { id: 'canvas' }, container = { ownerDocument: { defaultView: window } };
  const mounting = mountBrowserSession({
    wasm: { BrowserSession }, container, canvas, sessionId: SESSION, token: 'private-launch', recoveryState,
    requestTimeoutMs: timeout, pollMs: 60000, signal, capabilities, onError: message => errors.push(message), onBackendResult: result => results.push(result),
    onState: state => states.push(state), onSemantics: regions => semantics.push(regions),
    mountInteractiveSurface(options) { callbacks = options; options.onResize({ width: 90, height: 90, device_pixel_ratio: 1 }); return { setAccessibility(value) { accessibility.push(value); }, dispose() { calls.push('bridge-dispose'); }, focus() {} }; },
    async fetch(url, options) {
      assert.equal(url.origin, 'https://ui.example'); assert.equal(options.cache, 'no-store');
      assert.equal(options.redirect, 'error'); assert.equal(options.credentials, 'omit');
      assert.equal(options.referrerPolicy, 'no-referrer');
      if (options.body) bodies.push(options.body);
      return fetch ? fetch(url, options) : jsonResponse(options.body ? { submission_id: ID } : { epoch: 'first' });
    },
  });
  return { mounting, calls, errors, results, accessibility, instances, bodies, states, semantics, recoveryState, get callbacks() { return callbacks; } };
}

test('streamed byte budget and invalid UTF-8 fail closed', async () => {
  const r = response([new Uint8Array(3), new Uint8Array(3)]);
  await assert.rejects(readBounded(r, 5), /too large/);
  assert.equal(r.cancelled, true);
  await assert.rejects(readBounded(response([new Uint8Array([255])]), 100));
  await assert.rejects(readBounded(response([], { 'content-length': '999' }), 5), /too large/);
});

test('UTF-8 spans chunks and preserves exact u64 JSON', async () => {
  const text = '{"revision":18446744073709551615,"text":"🦀"}';
  assert.equal(await readBounded(response([...new TextEncoder().encode(text)].map(b => new Uint8Array([b]))), 1024), text);
});

test('aborting a stalled stream does not wait for its read or cancel', async () => {
  const controller = new AbortController();
  let cancelled = false;
  const r = { ok: true, headers: new Headers(), body: { getReader: () => ({ read: () => new Promise(() => {}), cancel() { cancelled = true; return new Promise(() => {}); }, releaseLock() {} }) } };
  const reading = readBounded(r, 100, controller.signal);
  controller.abort();
  await assert.rejects(reading, /aborted/); assert.equal(cancelled, true);
});

test('backend origins require explicit consent and secure transport', () => {
  const location = { origin: 'https://ui.example' };
  assert.equal(backendBase(undefined, location).origin, location.origin);
  assert.throws(() => backendBase('https://api.example', location));
  assert.equal(backendBase('https://api.example', location, ['https://api.example']).origin, 'https://api.example');
  for (const origin of ['https://user:pass@ui.example', 'https://ui.example/path', 'http://remote.example', 'file:///']) assert.throws(() => backendBase(origin, location));
});

test('ambiguous retries use exact payload and only matched receipt clears draft', async () => {
  let attempts = 0;
  const f = await fixture({ fetch: async (_, options) => {
    if (!options.body) return jsonResponse({ epoch: 'first' });
    attempts++;
    if (attempts === 1) throw new Error('private transport details');
    return jsonResponse({ submission_id: attempts === 2 ? 'wrong' : ID });
  } });
  const host = await f.mounting;
  f.callbacks.onInput({ text: 'draft 🦀', submit: true });
  await host.refresh();
  await assert.rejects(host.retrySubmission(), /unmatched/);
  await host.retrySubmission();
  assert.equal(new Set(f.bodies).size, 1); assert.equal(f.bodies.length, 3);
  assert.equal(f.instances[0].draft, '');
  assert.ok(f.errors.length > 0); assert.ok(f.errors.every(message => !message.includes('private transport')));
  await assert.rejects(host.retrySubmission(), /no unresolved/);
  await host.dispose(); await host.dispose();
  assert.equal(f.calls.filter(call => call === 'free').length, 1);
});

test('epoch remount preserves draft and unresolved submission with a fresh observation', async () => {
  let epoch = 'first';
  const f = await fixture({ fetch: async (_, options) => options.body ? Promise.reject(new Error('ambiguous')) : jsonResponse({ epoch }) });
  const host = await f.mounting;
  f.callbacks.onInput({ text: 'original', submit: true }); await host.refresh();
  const exact = f.instances[0].pending;
  f.callbacks.onInput({ text: 'newer draft' }); await host.refresh();
  epoch = 'restarted'; await host.refresh();
  assert.equal(f.instances.length, 2); assert.equal(f.instances[0].freed, true);
  assert.equal(f.instances[1].draft, 'newer draft'); assert.equal(f.instances[1].pending, exact);
  assert.equal(f.instances[1].epoch, 'restarted');
  await host.dispose();
  const next = await fixture({ recoveryState: f.recoveryState });
  const remounted = await next.mounting;
  assert.equal(next.instances[0].draft, 'newer draft'); assert.equal(next.instances[0].pending, exact);
  await remounted.dispose();
});

test('deadlines release stalled fetches and visible errors retain pending submission', async () => {
  const f = await fixture({ timeout: 10, fetch: async (_, options) => options.body ? new Promise(() => {}) : jsonResponse({ epoch: 'first' }) });
  const host = await f.mounting;
  f.callbacks.onInput({ text: 'draft', submit: true });
  await host.refresh();
  assert.ok(f.instances[0].pending); assert.ok(f.errors.length > 0);
  await host.dispose();
});

test('disposal aborts transport, ignores late results and rejects stale bridge callbacks', async () => {
  let release, blocked = false;
  const f = await fixture({ fetch: async () => blocked ? new Promise(resolve => { release = resolve; }) : jsonResponse({ epoch: 'first' }) });
  const host = await f.mounting;
  blocked = true;
  const refresh = host.refresh(); await tick();
  const count = f.calls.filter(call => call.startsWith('accept')).length;
  await host.dispose(); await refresh.catch(() => {});
  release(jsonResponse({ epoch: 'late' })); await tick();
  f.callbacks.onInput({ text: 'stale' }); f.callbacks.onResize({ width: 100, height: 100, device_pixel_ratio: 1 });
  assert.equal(f.callbacks.mapPoint({ x: 1, y: 1 }), null);
  assert.equal(f.calls.filter(call => call.startsWith('accept')).length, count);
  assert.equal(f.instances[0].freed, true);
});

test('bounded callback queue reports overflow and coalesces resize', async () => {
  const f = await fixture(); const host = await f.mounting;
  for (let i = 0; i < 1000; i++) {
    f.callbacks.onInput({ text: `${i}` });
    f.callbacks.onResize({ width: 90 + i, height: 90, device_pixel_ratio: 1 });
  }
  await tick(); await host.dispose();
  assert.ok(f.errors.length > 0);
  assert.ok(f.calls.filter(call => call === 'present').length < 70);
});

test('queued pointer coordinates cannot target a newer committed output', async () => {
  const f = await fixture(); const host = await f.mounting;
  assert.ok(f.callbacks.mapPoint({ x: 1, y: 1 }));
  f.callbacks.onInput({ type: 'text', text: 'changes output' });
  f.callbacks.onInput({ type: 'pointer', action: 'down', x: 1, y: 1 });
  await host.refresh();
  assert.equal(f.calls.includes('input:pointer'), false);
  assert.ok(f.calls.includes('cancel'));
  await host.dispose();
});

test('external lifetime abort cancels an initializing mount', async () => {
  const controller = new AbortController();
  const f = await fixture({ signal: controller.signal, fetch: () => new Promise(() => {}) });
  await tick(); controller.abort();
  await assert.rejects(f.mounting, /initialization failed/);
  assert.equal(f.instances[0].freed, true);
});

test('backend JSON error envelopes are visible without clearing recovery', async () => {
  const f = await fixture({ fetch: async (_, options) => jsonResponse(options.body ? { schema_version: 1, error: 'submission_outcome_unknown' } : { epoch: 'first' }) });
  const host = await f.mounting;
  f.callbacks.onInput({ text: 'retain me', submit: true }); await host.refresh();
  assert.ok(f.errors.length > 0); assert.ok(f.instances[0].pending);
  await host.dispose();
});

test('chunk work budget bounds streams of empty chunks', async () => {
  await assert.rejects(readBounded(response(Array.from({ length: 32769 }, () => new Uint8Array())), 100), /chunk budget/);
});

test('history seam passes original u64 request bytes and accepts semantic replacement', async () => {
  let historyBody;
  const f = await fixture({ fetch: async (url, options) => {
    if (url.pathname.endsWith('/history')) { historyBody = options.body; return jsonResponse({ epoch: 'first', revision: 'history-window' }); }
    return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  await host.loadHistory('older');
  assert.match(historyBody, /"revision":18446744073709551615/);
  assert.equal(JSON.parse(historyBody).direction, 'older');
  assert.ok(f.calls.includes('history:older'));
  assert.equal(f.results[0].type, 'history');
  await assert.rejects(host.loadHistory('arbitrary-path'), /unavailable/);
  await host.dispose();
});

test('typed controls validate response and refresh without automatic retry', async () => {
  const attempts = [];
  let reads = 0;
  const f = await fixture({ fetch: async (url, options) => {
    if (url.pathname.endsWith('/control')) {
      attempts.push(options.body);
      if (attempts.length === 1) throw new Error('ambiguous control');
      return jsonResponse({ schema_version: 1, outcome: { type: 'permission_resolved', resolved: true } });
    }
    reads++; return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  await assert.rejects(host.control('{"type":"permission","permission_id":"p","approved":true,"remember":false}'), /outcome may be unknown/);
  assert.equal(attempts.length, 1); assert.equal(reads, 2);
  assert.ok(f.errors.some(message => message.includes('no automatic retry')));
  await assert.rejects(host.control('{"type":"permission","permission_id":"p","approved":false,"remember":false}'), /unresolved/);
  assert.equal(attempts.length, 1);
  await host.acknowledgeControl();
  await host.control('{"type":"permission","permission_id":"p","approved":false,"remember":false}');
  assert.match(attempts[1], /"revision":18446744073709551615/);
  assert.equal(reads, 4); assert.equal(f.results[0].type, 'control');
  await assert.rejects(host.control('{"type":"invoke_skill"}'), /invalid browser control/);
  assert.equal(attempts.length, 2);
  await host.dispose();
});

test('unrelated control outcome fails closed and still refreshes', async () => {
  let reads = 0;
  const f = await fixture({ fetch: async (url) => {
    if (url.pathname.endsWith('/control')) return jsonResponse({ schema_version: 1, outcome: { type: 'submission_accepted' } });
    reads++; return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  await assert.rejects(host.control('{"type":"interaction","interaction_id":"i","input":{"type":"cancel"}}'), /outcome may be unknown/);
  assert.equal(reads, 2); assert.equal(f.results.length, 0);
  assert.equal(f.states.at(-1).controlUncertain, true);
  await assert.rejects(host.control('{"type":"interaction","interaction_id":"i","input":{"type":"cancel"}}'), /Previous control outcome/);
  assert.equal(reads, 2);
  await host.dispose();
});

test('input drains typed Rust history intents and updates bridge from committed metadata', async () => {
  const intents = [{ type: 'history', direction: 'older' }];
  const paths = [];
  const f = await fixture({ intents, fetch: async (url) => { paths.push(url.pathname); return jsonResponse({ epoch: 'first' }); } });
  const host = await f.mounting;
  f.callbacks.onInput({ type: 'key' }); await host.refresh();
  assert.equal(paths.filter(path => path.endsWith('/history')).length, 1);
  assert.equal(f.accessibility.at(-1).label, 'Committed session');
  assert.match(f.accessibility.at(-1).description, /New messages below/);
  await host.dispose();
});

test('input dispatches safe control intent and never assumes platform capabilities', async () => {
  const intents = [{ type: 'control', action_json: '{"type":"permission","permission_id":"p","approved":false,"remember":false}' }];
  let controls = 0;
  const f = await fixture({ intents, fetch: async (url) => {
    if (url.pathname.endsWith('/control')) { controls++; return jsonResponse({ schema_version: 1, outcome: { type: 'permission_resolved', resolved: true } }); }
    return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  f.callbacks.onInput({ type: 'key' }); await host.refresh(); assert.equal(controls, 1);
  intents.push({ type: 'navigate', url: 'https://example.com' });
  f.callbacks.onInput({ type: 'key' }); await host.refresh(); assert.ok(f.errors.length);
  await host.dispose();
});

test('navigation and clipboard intents use injected authority and reject unsafe URLs', async () => {
  const opened = [], copied = [], intents = [{ type: 'navigate', url: 'https://example.com/page' }, { type: 'clipboard', text: 'selection' }];
  const f = await fixture({ intents, capabilities: { navigate: url => opened.push(url), writeClipboard: text => copied.push(text) } });
  const host = await f.mounting;
  f.callbacks.onInput({ type: 'key' }); await host.refresh();
  assert.deepEqual(opened, ['https://example.com/page']); assert.deepEqual(copied, ['selection']);
  intents.push({ type: 'navigate', url: 'javascript:alert(1)' });
  f.callbacks.onInput({ type: 'key' }); await host.refresh();
  assert.equal(opened.length, 1); assert.ok(f.errors.length);
  await host.dispose();
});

test('uncommitted output cannot publish accessibility or enable pointer mapping', async () => {
  const f = await fixture({ fault: 'uncommitted' });
  const host = await f.mounting;
  assert.deepEqual(f.accessibility, [{ label: 'Bcode session', description: '' }]);
  assert.equal(f.callbacks.mapPoint({ x: 1, y: 1 }), null);
  await host.dispose();
});

test('initialization failure releases constructed WASM', async () => {
  const f = await fixture({ fault: 'configure' });
  await assert.rejects(f.mounting, /initialization failed/);
  assert.equal(f.instances[0].freed, true);
});

test('ordinary submit never replays an unresolved operation and recovery exposes explicit retry', async () => {
  let attempts = 0;
  const f = await fixture({ fetch: async (url) => {
    if (url.pathname.endsWith('/submit-message')) { attempts++; throw new Error('ambiguous'); }
    return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  try {
    f.callbacks.onInput({ text: 'retained', submit: true });
    await host.refresh();
    assert.equal(attempts, 1);
    assert.equal(f.states.at(-1).unresolvedSubmission, true);
    f.callbacks.onInput({ submit: true });
    await host.refresh();
    assert.equal(attempts, 1);
    await assert.rejects(host.retrySubmission());
    assert.equal(attempts, 2);
  } finally { await host.dispose(); }
  assert.deepEqual(f.semantics.at(-1), []);
});

test('unknown control lock survives remount and explicit acknowledgement sends no control', async () => {
  const recoveryState = {};
  const f = await fixture({ recoveryState, fetch: async url => {
    if (url.pathname.endsWith('/control')) throw new Error('unknown');
    return jsonResponse({ epoch: 'first' });
  } });
  const first = await f.mounting;
  await assert.rejects(first.control('{"type":"permission","permission_id":"p","approved":true,"remember":false}'));
  await first.dispose();
  assert.equal(recoveryState.controlUncertain, true);
  const next = await fixture({ recoveryState });
  const host = await next.mounting;
  try {
    await assert.rejects(host.control('{"type":"permission","permission_id":"p","approved":false,"remember":false}'), /unresolved/);
    assert.equal(next.bodies.length, 0);
    await host.acknowledgeControl();
    assert.equal(next.bodies.length, 0);
    assert.equal(next.states.at(-1).controlUncertain, false);
  } finally { await host.dispose(); }
});

test('artifact image decoder accepts bounded raster only and releases resources', async () => {
  let closed = 0;
  const bitmap = { width:2, height:1, close() { closed++; } };
  const environment = {
    async createImageBitmap(blob, options) { assert.equal(blob.type,'image/png'); assert.equal(options.resizeWidth,512); return bitmap; },
    OffscreenCanvas: class { getContext() { return { drawImage() {}, getImageData() { return { data:new Uint8ClampedArray(8) }; } }; } },
  };
  const decoded = await decodeArtifactImage(new Uint8Array([1]), 'image/png', environment);
  assert.equal(decoded.bytes.length,8); assert.equal(closed,1);
  await assert.rejects(decodeArtifactImage(new Uint8Array([1]), 'image/svg+xml', environment), /unavailable/);
  bitmap.width=4096;
  await assert.rejects(decodeArtifactImage(new Uint8Array([1]), 'image/png', environment), /budget/);
  assert.equal(closed,2);
});

test('queued keyboard input cannot activate a replacement canonical control', async () => {
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  let reads = 0;
  const f = await fixture({ fetch: async () => {
    if (++reads === 2) await gate;
    return jsonResponse({ epoch: 'first', revision: reads });
  } });
  const host = await f.mounting;
  try {
    const refresh = host.refresh();
    await tick();
    f.callbacks.onInput({ type:'key', key:'Enter', code:'Enter' });
    release();
    await refresh;
    await host.refresh();
    assert.equal(f.calls.filter(call => call === 'input:key').length, 0);
  } finally { release(); await host.dispose(); }
});

test('artifact port uses fenced requests, exact offsets, bounded bytes and no paths', async () => {
  let body;
  const f = await fixture({ fetch: async (url, options) => {
    if (url.pathname.endsWith('/artifact')) {
      body = options.body;
      return jsonResponse({ reference_key: 'image', content_type: 'image/png', bytes: [137,80,78,71], total_bytes: '4' });
    }
    return jsonResponse({ epoch: 'first' });
  } });
  const host = await f.mounting;
  try {
    const range = await host.readArtifact('item', 'image', 18446744073709551615n, 16);
    assert.match(body, /"offset":18446744073709551615/);
    assert.deepEqual([...range.bytes], [137,80,78,71]);
    assert.equal(range.contentType, 'image/png');
    await assert.rejects(host.readArtifact('item', 'image', 1), /invalid/);
    await assert.rejects(host.readArtifact('item', 'image', 0n, 131073), /invalid/);
  } finally { await host.dispose(); }
});

test('committed semantic mirror is atomic, passive, text-only and clears on empty frame', () => {
  const root = {
    nodes: [],
    ownerDocument: { createElement() { return { dataset: {}, attributes: {}, setAttribute(key, value) { this.attributes[key] = value; } }; } },
    replaceChildren(...nodes) { this.nodes = nodes; },
  };
  const region = { id: 'question/one', role: 'button', area: { x: 1, y: 1, width: 2, height: 1 }, label: '<script>', text: 'literal', disabled: true, focused: true };
  commitSemanticRegions(root, [region]);
  assert.equal(root.nodes[0].attributes['aria-label'], '<script>');
  assert.equal(root.nodes[0].textContent, 'literal');
  assert.equal(root.nodes[0].onclick, undefined);
  assert.equal(root.nodes[0].attributes['aria-disabled'], 'true');
  const previous = root.nodes;
  assert.throws(() => commitSemanticRegions(root, [region, region]), /invalid/);
  assert.equal(root.nodes, previous);
  assert.throws(() => commitSemanticRegions(root, [{ ...region, area: { ...region.area, width: 0 } }]), /invalid/);
  commitSemanticRegions(root, []);
  assert.deepEqual(root.nodes, []);
});
