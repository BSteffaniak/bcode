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

test('opens explicit incomplete recording without any live bootstrap', async ({ page }) => {
  let bootstraps = 0;
  await page.route('**/__xray/bootstrap', route => { bootstraps++; return route.abort(); });
  await page.goto('/');
  await page.getByLabel('Open recording files').setInputFiles({ name: 'events.jsonl', mimeType: 'application/x-ndjson', buffer: Buffer.from(JSON.stringify({ format: 'bcode-xray-inspector', schema_version: 1, coverage: 'incomplete_retained_window', payloads: 'not_included', sensitive: true }) + '\n' + JSON.stringify(event(91, 'offline-sentinel')) + '\n') });
  await expect(page.getByText('offline-sentinel', { exact: true }).first()).toBeVisible();
  expect(bootstraps).toBe(0);
});
