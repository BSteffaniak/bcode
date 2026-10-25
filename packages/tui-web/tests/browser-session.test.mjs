import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
// Source module has no Node/package dependencies; packaging selects its module URL.
const source = await readFile(new URL('../src/browser-session.js', import.meta.url), 'utf8');
const { readBounded, backendBase, mountBrowserSession } = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

function response(chunks, headers = {}) {
  let cancelled = false;
  return {
    ok: true, headers: new Headers(headers),
    body: { getReader: () => ({
      async read() { return chunks.length ? { value: chunks.shift(), done: false } : { done: true }; },
      async cancel() { cancelled = true; }, releaseLock() {},
    }) },
    get cancelled() { return cancelled; },
  };
}

test('streamed byte budget enforced without content length and reader cancelled', async () => {
  const r = response([new Uint8Array(3), new Uint8Array(3)]);
  await assert.rejects(readBounded(r, 5), /too large/);
  assert.equal(r.cancelled, true);
});

test('UTF-8 decoding spans chunks and preserves exact u64 revision JSON', async () => {
  const text = '{"revision":18446744073709551615,"text":"🦀"}';
  const bytes = new TextEncoder().encode(text);
  assert.equal(await readBounded(response([...bytes].map(b => new Uint8Array([b]))), 1024), text);
  await assert.rejects(readBounded(response([new Uint8Array([255])]), 100));
});

test('backend origins require explicit cross-origin consent and reject credentials', () => {
  const location = { origin: 'https://ui.example' };
  assert.equal(backendBase(undefined, location).origin, location.origin);
  assert.throws(() => backendBase('https://api.example', location));
  assert.equal(backendBase('https://api.example', location, ['https://api.example']).origin, 'https://api.example');
  for (const origin of ['http://remote.example', 'https://u:p@api.example', 'https://api.example/path', 'file:///tmp']) {
    assert.throws(() => backendBase(origin, location, ['https://api.example']));
  }
});

test('host serializes WASM async calls, bounds fetch policy, retries exact body and disposes', async () => {
  const calls = [], bodies = [];
  let bridgeCallbacks;
  let active = false, freed = false, aborted = false;
  const exact = '{"submission_id":"once","text":"draft"}';
  let pending = null;
  let firstSubmit = true;
  class BrowserSession {
    configure_bindings() {}
    take_clipboard() { return null; }
    resize() { assert.equal(active, false); calls.push('resize'); }
    accept_snapshot(value) { assert.equal(active, false); assert.equal(value, 'snapshot'); }
    input() { assert.equal(active, false); return true; }
    async present() { assert.equal(active, false); active = true; await new Promise(resolve => setTimeout(resolve, 2)); active = false; calls.push('present'); }
    retry_submission() { return pending; }
    prepare_submission() { pending = exact; return exact; }
    settle_submission() { pending = null; calls.push('settled'); }
    cancel_pointer() { assert.equal(active, false); calls.push('cancel'); }
    dispose() { assert.equal(active, false); calls.push('dispose'); }
    free() { freed = true; }
  }
  const window = { location: { origin: 'https://ui.example' }, crypto: { randomUUID: () => 'once' }, setTimeout: () => 1, clearTimeout() {} };
  const canvas = { id: 'canvas' }, container = { ownerDocument: { defaultView: window } };
  const host = await mountBrowserSession({
    wasm: { BrowserSession }, container, canvas,
    sessionId: '00000000-0000-4000-8000-000000000001', token: 'private-launch',
    mountInteractiveSurface(options) { bridgeCallbacks = options; options.onResize({ width: 90, height: 90, device_pixel_ratio: 1 }); return { setAccessibility() {}, dispose() { calls.push('bridge-dispose'); }, focus() {} }; },
    async fetch(url, options) {
      assert.equal(url.origin, 'https://ui.example');
      assert.equal(options.cache, 'no-store');
      assert.equal(options.redirect, 'error');
      assert.equal(options.credentials, 'omit');
      options.signal.addEventListener('abort', () => { aborted = true; });
      if (options.body) {
        bodies.push(options.body);
        if (firstSubmit) { firstSubmit = false; throw new Error('ambiguous'); }
      }
      return response([new TextEncoder().encode(options.body ? 'receipt' : 'snapshot')]);
    },
  });
  bridgeCallbacks.onInput({ type: 'key', key: 'Enter' });
  await host.refresh();
  await host.retrySubmission();
  assert.deepEqual(bodies, [exact, exact]);
  await host.dispose();
  assert.equal(freed, true);
  assert.equal(aborted, true);
  assert.ok(calls.indexOf('bridge-dispose') < calls.indexOf('dispose'));
  await host.dispose();
});
