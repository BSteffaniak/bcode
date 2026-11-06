import { describe, expect, it, vi } from 'vitest';
import type { InspectorBootstrap, XrayEnvelope } from '../contracts';
import { ObservationTransport, validateBootstrap } from './transport';
import { StoreBridge, type WorkerPort } from './bridge';
import { InspectorWorkerEngine, type WorkerCommand, type WorkerReply } from './store';
import { InspectorClient } from './client';
const bootstrap: InspectorBootstrap = { schema_version: 1, endpoint: 'http://127.0.0.1:1234/', bearer: 'test-only', session_id: 's', deep_capture: false };
const readyData = { schema_version: 1, deep_capture: false, durable_replay: false, queue_capacity: 10, max_event_bytes: 1000, max_payload_bytes: 1000, max_retained_payload_bytes: 1000, max_retained_payloads: 10, max_payload_range_bytes: 64 };
const receipt = { observation_id: 'o', final_sequence: 2, dropped_events: 0, hub_contention_drops: 0, evicted_payloads: 0 };
function control(sequence: number, name: string, data: unknown): XrayEnvelope { return { schema_version: 1, observation_id: 'o', sequence, timestamp_unix_ms: 0, event: { domain: 'xray', name, correlation: { session_id: 's' }, sensitive: false, data } }; }
function fixture(deep = false) {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const stream = new ReadableStream<Uint8Array>({ start(c) { controller = c; } });
  const send = (e: XrayEnvelope) => controller.enqueue(new TextEncoder().encode(`event: xray\ndata: ${JSON.stringify(e)}\n\n`));
  const calls: { url: string; init?: RequestInit }[] = [];
  let payload: unknown;
  const fetcher = (async (input: URL | RequestInfo, init?: RequestInit) => {
    const url = String(input); calls.push({ url, init });
    if (url.endsWith('/observe')) { send(control(1, 'ready', { ...readyData, deep_capture: deep })); return new Response(stream, { headers: { 'Content-Type': 'text/event-stream' } }); }
    if (url.endsWith('/finalize')) { send(control(2, 'finalized', receipt)); return Response.json(receipt); }
    if (url.endsWith('/release')) return new Response(null, { status: 204 });
    if (url.endsWith('/payload')) return Response.json(payload);
    throw new Error('Unexpected route');
  }) as typeof fetch;
  return { fetcher, calls, send, close: () => controller.close(), setPayload: (value: unknown) => { payload = value; } };
}
const hooks = () => ({ events: vi.fn(async (_events: XrayEnvelope[]) => {}), state: vi.fn(), issue: vi.fn() });
describe('authenticated one-way live transport', () => {
  it('resolves at readiness while stream stays open; finalize drains then releases', async () => {
    const f = fixture(); const h = hooks(); const t = new ObservationTransport(bootstrap, h, f.fetcher);
    await t.connect(); expect(h.state).toHaveBeenCalledWith('live'); expect(f.calls).toHaveLength(1);
    await t.stop(); expect(h.events.mock.calls.flatMap(([e]) => e).map(e => e.event.name)).toEqual(['ready', 'finalized']);
    expect(f.calls.map(c => c.url.split('/').at(-1))).toEqual(['observe', 'finalize', 'release']);
    expect(JSON.parse(f.calls[1].init!.body as string)).toEqual({ schema_version: 1, observation_id: 'o' });
    for (const call of f.calls) expect(call.init).toMatchObject({ method: 'POST', credentials: 'omit', redirect: 'error', cache: 'no-store', referrerPolicy: 'no-referrer', headers: { Authorization: 'Bearer test-only' } });
  });
  it('reports EOF interruption without retrying or claiming execution termination', async () => {
    const f = fixture(); const h = hooks(); let disconnected!: () => void; const done = new Promise<void>(resolve => { disconnected = resolve; });
    h.state.mockImplementation(state => { if (state === 'disconnected') disconnected(); });
    const t = new ObservationTransport(bootstrap, h, f.fetcher); await t.connect(); f.close(); await done;
    expect(f.calls).toHaveLength(1); expect(h.issue).toHaveBeenCalled(); t.dispose();
  });
  it('fails incompatible readiness before reporting ready', async () => {
    const f = fixture(true); const t = new ObservationTransport(bootstrap, hooks(), f.fetcher); await expect(t.connect()).rejects.toThrow(); t.dispose();
  });
  it('permits only declared typed payloads and verifies range length and digest', async () => {
    const f = fixture(true); let ingested!: () => void; const arrived = new Promise<void>(resolve => { ingested = resolve; }); const h = hooks();
    h.events.mockImplementation(async events => { if (events.some(e => e.sequence === 2)) ingested(); });
    const t = new ObservationTransport({ ...bootstrap, deep_capture: true }, h, f.fetcher); await t.connect();
    const ref = { schema_version: 1, payload_id: 'p', session_id: 's', content_type: 'text/plain', byte_len: 0, sha256: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855' };
    await expect(t.readPayload(ref)).rejects.toThrow();
    const event = control(2, 'capture', { payload: ref }); event.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/payload' }] }; f.send(event); await arrived;
    f.setPayload({ schema_version: 1, observation_id: 'o', payload: ref, offset: 0, bytes: [], eof: true }); expect(await t.readPayload(ref)).toEqual(new Uint8Array());
    await expect(t.readPayload({ ...ref, sha256: '0'.repeat(64) })).rejects.toThrow();
    f.setPayload({ schema_version: 1, observation_id: 'o', payload: ref, offset: 1, bytes: [], eof: true }); await expect(t.readPayload(ref)).rejects.toThrow();
    t.dispose(); await expect(t.readPayload(ref)).rejects.toThrow();
  });
  it('rejects URL credentials, remote hosts, query credentials and unknown bootstrap versions', () => {
    for (const endpoint of ['http://localhost/', 'https://127.0.0.1/', 'http://127.0.0.1/?bearer=x', 'http://secret@127.0.0.1/', 'http://127.0.0.1/path']) expect(() => validateBootstrap({ ...bootstrap, endpoint })).toThrow();
    expect(() => validateBootstrap({ ...bootstrap, schema_version: 2 } as unknown as InspectorBootstrap)).toThrow();
  });
});
class InlinePort implements WorkerPort {
  onmessage: WorkerPort['onmessage'] = null; onerror: WorkerPort['onerror'] = null;
  readonly engine = new InspectorWorkerEngine();
  postMessage(command: WorkerCommand): void { queueMicrotask(() => this.onmessage?.({ data: this.engine.handle(command) } as MessageEvent<WorkerReply>)); }
  terminate(): void {}
}
describe('bounded worker integration', () => {
  it('enforces pending-control bound and settles acknowledged requests', async () => {
    const port = new InlinePort(); const bridge = new StoreBridge(port, () => {});
    const requests = Array.from({ length: 8 }, () => bridge.send({ kind: 'state', state: 'idle' }));
    await expect(bridge.send({ kind: 'state', state: 'idle' })).rejects.toThrow('queue'); await Promise.all(requests); bridge.dispose();
  });
  it('concrete client connects through worker and clears live data for recordings', async () => {
    const port = new InlinePort(); const f = fixture(); const snapshots: unknown[] = [];
    const client = new InspectorClient(s => snapshots.push(s), { worker: port, fetch: f.fetcher });
    await client.connect(bootstrap); await client.stop(); expect(port.engine.store.snapshot().retained).toBe(2);
    let recording!: () => void; const done = new Promise<void>(resolve => { recording = resolve; });
    const port2 = new InlinePort(); const offline = new InspectorClient(s => { if (s.state === 'recording' && s.received === 1) recording(); }, { worker: port2 });
    offline.loadRecording([control(1, 'ready', readyData)]); await done; expect(port2.engine.store.snapshot()).toMatchObject({ scope: 'opened-recording', received: 1 });
    client.dispose(); offline.dispose(); expect(snapshots.length).toBeGreaterThan(0);
  });
});
