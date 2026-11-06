import { test, expect } from '@playwright/test';

const event = (sequence: number, name: string, domain = 'provider', data: unknown = {}) => ({
  schema_version: 1, observation_id: 'browser-observation', sequence, timestamp_unix_ms: 1700000000000 + sequence,
  event: { domain, name, correlation: { session_id: 'browser-session', request_id: 'request-1' }, sensitive: false, data },
});

test('pairs without persistent secrets, ingests through worker, filters and inspects untrusted evidence', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/__xray/bootstrap', async route => {
    expect(route.request().method()).toBe('POST');
    if (!route.request().headers()['x-xray-pairing-code']) { await route.fulfill({ status: 403 }); return; }
    expect(route.request().headers()['x-xray-pairing-code']).toBe('browser-test-code');
    await route.fulfill({ json: { schema_version: 1, endpoint: 'http://127.0.0.1:4173/', bearer: 'test-memory-only', session_id: 'browser-session', deep_capture: false } });
  });
  await page.route('**/v1/xray/observe', async route => {
    expect(route.request().headers().authorization).toBe('Bearer test-memory-only');
    const events = [
      event(1, 'ready', 'xray', { schema_version: 1, deep_capture: false, durable_replay: false, queue_capacity: 100, max_event_bytes: 100000, max_payload_bytes: 10000, max_retained_payload_bytes: 10000, max_retained_payloads: 10, max_payload_range_bytes: 1000 }),
      event(2, 'response_delta', 'provider', { text: '<script>window.pwned=true</script> browser-sentinel' }),
      event(3, 'tool_finished', 'tool', { outcome: 'failed', message: 'controlled failure' }),
    ];
    await route.fulfill({ contentType: 'text/event-stream', body: events.map(value => `event: xray\ndata: ${JSON.stringify(value)}\n\n`).join('') });
  });
  await page.goto('/');
  await page.getByLabel('CLI pairing code').fill('browser-test-code');
  await page.getByRole('button', { name: 'Connect live' }).click();
  await expect(page.getByLabel('CLI pairing code')).toHaveValue('');
  await expect(page.getByText('response_delta', { exact: true }).first()).toBeVisible();
  const search = page.getByLabel('Search retained evidence');
  await search.fill('domain:provider');
  await expect(page.getByText('tool_finished', { exact: true })).toHaveCount(0);
  await page.getByText('response_delta', { exact: true }).first().click();
  await expect(page.getByText(/browser-sentinel/).first()).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { pwned?: boolean }).pwned)).toBeUndefined();
  expect(await page.evaluate(() => JSON.stringify(localStorage))).not.toContain('test-memory-only');
  expect(await page.evaluate(() => JSON.stringify(localStorage))).not.toContain('browser-test-code');
  await search.fill('unsupported:value');
  await expect(page.getByText(/unknown.*field|unsupported.*field/i).first()).toBeVisible();
  expect(errors).toEqual([]);
});

test('auto attaches a fragment ticket once in StrictMode after clearing history, with a visible demo label', async ({ page }) => {
  let bootstraps = 0;
  let release!: () => void;
  const barrier = new Promise<void>(resolve => { release = resolve; });
  await page.addInitScript(() => {
    const replace = history.replaceState.bind(history);
    history.replaceState = (state, unused, url) => {
      if (location.hash.startsWith('#xray-ticket=')) {
        (window as unknown as { clearedBeforeMount: boolean }).clearedBeforeMount = !document.getElementById('root')?.hasChildNodes();
      }
      replace(state, unused, url);
    };
  });
  await page.route('**/__xray/bootstrap', async route => {
    bootstraps++;
    if (!route.request().headers()['x-xray-pairing-code']) { await route.fulfill({ status: 403 }); return; }
    expect(route.request().headers()['x-xray-pairing-code']).toBe('fragment-ticket');
    expect(route.request().headers().cookie).toBeUndefined();
    expect(await page.evaluate(() => location.hash)).toBe('');
    await barrier;
    await route.fulfill({ json: { schema_version: 1, endpoint: '/', bearer: 'gateway-memory-only', session_id: 'browser-session', deep_capture: false, demo: true } });
  });
  await page.route('**/v1/xray/observe', async route => {
    expect(new URL(route.request().url()).origin).toBe(new URL(page.url()).origin);
    expect(route.request().headers().authorization).toBe('Bearer gateway-memory-only');
    const events = [
      event(1, 'ready', 'xray', { schema_version: 1, deep_capture: false, durable_replay: false, queue_capacity: 100, max_event_bytes: 100000, max_payload_bytes: 10000, max_retained_payload_bytes: 10000, max_retained_payloads: 10, max_payload_range_bytes: 1000 }),
      event(2, 'auto-attached'),
    ];
    await route.fulfill({ contentType: 'text/event-stream', body: events.map(value => `event: xray\ndata: ${JSON.stringify(value)}\n\n`).join('') });
  });
  await page.goto('/#xray-ticket=fragment-ticket');
  await expect(page.getByText('bootstrapping', { exact: true })).toBeVisible();
  expect(await page.evaluate(() => location.hash)).toBe('');
  expect(await page.evaluate(() => (window as unknown as { clearedBeforeMount: boolean }).clearedBeforeMount)).toBe(true);
  release();
  await expect(page.getByText('auto-attached', { exact: true }).first()).toBeVisible();
  await expect(page.getByText('Synthetic demo', { exact: true })).toBeVisible();
  expect(bootstraps).toBe(1);
  expect(await page.evaluate(() => JSON.stringify([localStorage, sessionStorage, history.state]))).not.toMatch(/fragment-ticket|gateway-memory-only/);
  await page.reload();
  await expect(page.getByRole('button', { name: 'Connect live' })).toBeVisible();
  expect(bootstraps).toBe(2);
});

for (const failure of ['expired ticket', 'wrong gateway bearer', 'foreign endpoint'] as const) {
  test(`auto launch fails closed: ${failure}`, async ({ page }) => {
    let bootstraps = 0; let observations = 0; let foreign = 0;
    await page.route('https://foreign.invalid/**', route => { foreign++; return route.abort(); });
    await page.route('**/__xray/bootstrap', route => {
      bootstraps++;
      return failure === 'expired ticket'
        ? route.fulfill({ status: 403, body: 'private-expiry-detail' })
        : route.fulfill({ json: { schema_version: 1, endpoint: failure === 'foreign endpoint' ? 'https://foreign.invalid/' : '/', bearer: 'wrong-gateway-bearer', session_id: 'browser-session', deep_capture: false } });
    });
    await page.route('**/v1/xray/observe', route => {
      observations++;
      expect(route.request().headers().authorization).toBe('Bearer wrong-gateway-bearer');
      return route.fulfill({ status: 401, body: 'private-auth-detail' });
    });
    await page.goto('/#xray-ticket=one-use-ticket');
    await expect(page.getByText(/This launch link expired/)).toBeVisible();
    await expect(page.getByText('disconnected', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Connect live' })).toBeVisible();
    expect(await page.evaluate(() => location.hash)).toBe('');
    expect(await page.locator('body').innerText()).not.toMatch(/private-expiry-detail|private-auth-detail|wrong-gateway-bearer|one-use-ticket/);
    expect(await page.evaluate(() => JSON.stringify([localStorage, sessionStorage, history.state]))).not.toMatch(/one-use-ticket|wrong-gateway-bearer/);
    expect(bootstraps).toBe(1);
    expect(observations).toBe(failure === 'wrong gateway bearer' ? 1 : 0);
    expect(foreign).toBe(0);
    await page.reload();
    await expect(page.getByRole('button', { name: 'Connect live' })).toBeVisible();
    await expect.poll(() => bootstraps).toBe(2);
  });
}

test('browser worker failure is friendly and does not spend the ticket', async ({ page }) => {
  let bootstraps = 0;
  await page.addInitScript(() => {
    window.Worker = class { constructor() { throw new Error('private-browser-detail'); } } as unknown as typeof Worker;
  });
  await page.route('**/__xray/bootstrap', route => { bootstraps++; return route.fulfill({ status: 403 }); });
  await page.goto('/#xray-ticket=unspent-ticket');
  await expect(page.getByText(/This launch link expired/)).toBeVisible();
  await expect(page.getByText('disconnected', { exact: true })).toBeVisible();
  expect(await page.evaluate(() => location.hash)).toBe('');
  expect(await page.locator('body').innerText()).not.toContain('private-browser-detail');
  expect(bootstraps).toBe(0);
});

test('opens explicit incomplete recording after an unavailable local host probe', async ({ page }) => {
  let bootstraps = 0;
  await page.route('**/__xray/bootstrap', route => { bootstraps++; return route.fulfill({ status: 403 }); });
  await page.goto('/');
  await page.getByLabel('Open recording files').setInputFiles({ name: 'events.jsonl', mimeType: 'application/x-ndjson', buffer: Buffer.from(JSON.stringify({ format: 'bcode-xray-inspector', schema_version: 1, coverage: 'incomplete_retained_window', payloads: 'not_included', sensitive: true }) + '\n' + JSON.stringify(event(91, 'offline-sentinel')) + '\n') });
  await expect(page.getByText('offline-sentinel', { exact: true }).first()).toBeVisible();
  expect(bootstraps).toBe(1);
});


test('open localhost connects without a code, ticket or persistent credential', async ({ page }) => {
  let bootstraps = 0;
  await page.route('**/__xray/bootstrap', async route => {
    bootstraps++;
    expect(route.request().headers()['x-xray-pairing-code']).toBeUndefined();
    await route.fulfill({ json: { schema_version: 1, endpoint: '/', bearer: 'local-memory-only', session_id: 'browser-session', deep_capture: false } });
  });
  await page.route('**/v1/xray/observe', async route => {
    expect(route.request().headers().authorization).toBe('Bearer local-memory-only');
    await route.fulfill({ contentType: 'text/event-stream', body: `event: xray\ndata: ${JSON.stringify(event(1, 'ready', 'xray', { schema_version: 1, deep_capture: false, durable_replay: false, queue_capacity: 100, max_event_bytes: 100000, max_payload_bytes: 100000, max_retained_payload_bytes: 100000, max_retained_payloads: 10, max_payload_range_bytes: 10000 }))}\n\n` });
  });
  await page.goto('/');
  await expect(page.getByText('ready', { exact: true }).first()).toBeVisible();
  expect(bootstraps).toBe(1);
  await expect(page.getByLabel('CLI pairing code')).toHaveCount(0);
  expect(new URL(page.url()).hash).toBe('');
  expect(await page.evaluate(() => JSON.stringify(localStorage))).not.toContain('local-memory-only');
});
