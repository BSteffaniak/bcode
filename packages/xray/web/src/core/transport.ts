import { Sha256 } from '../recording/sha256';
import type { InspectorBootstrap, XrayEnvelope } from '../contracts';
import { declaredReferences, envelope, MAX_FRAME_BYTES, MAX_PAYLOAD_BYTES, object, payloadRef, refKey, SequenceTracker, SseParser, uint, validateFinalization, type PayloadRef } from './protocol';
export interface TransportHooks { events: (events: XrayEnvelope[]) => Promise<void>; state: (state: 'live' | 'stopped' | 'disconnected' | 'finalizing') => void; issue: (message: string) => void }
export function validateBootstrap(b: InspectorBootstrap, browserOrigin = globalThis.location?.origin): URL {
  // Only '/' is a supported relative endpoint. Never resolve arbitrary relative or protocol-relative URLs.
  const u = new URL(b.endpoint === '/' && browserOrigin ? `${browserOrigin}/` : b.endpoint);
  const sameOrigin = browserOrigin !== undefined && u.origin === browserOrigin && ['http:', 'https:'].includes(u.protocol);
  const loopback = u.protocol === 'http:' && ['127.0.0.1', '[::1]'].includes(u.hostname);
  if (b.schema_version !== 1 || typeof b.bearer !== 'string' || !b.bearer || /[^\x21-\x7e]/u.test(b.bearer) || typeof b.session_id !== 'string' || !b.session_id || typeof b.deep_capture !== 'boolean' || (b.demo !== undefined && typeof b.demo !== 'boolean') || (!sameOrigin && !loopback) || u.pathname !== '/' || u.search || u.hash || u.username || u.password) throw new Error('Invalid provisioned observation bootstrap');
  return u;
}
async function boundedJson(response: Response, max = MAX_FRAME_BYTES): Promise<unknown> {
  if (!response.ok || !response.body) throw new Error('Observation request unavailable');
  const reader = response.body.getReader(); const chunks: Uint8Array[] = []; let size = 0;
  try { for (;;) { const { value, done } = await reader.read(); if (done) break; size += value.length; if (size > max) throw new Error('Response exceeds byte bound'); chunks.push(value); } }
  finally { await reader.cancel().catch(() => undefined); reader.releaseLock(); }
  const bytes = new Uint8Array(size); let offset = 0; for (const c of chunks) { bytes.set(c, offset); offset += c.length; }
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
}
export class ObservationTransport {
  private readonly origin: URL;
  private readonly abort = new AbortController();
  private observation?: string;
  private tracker = new SequenceTracker();
  private refs = new Map<string, PayloadRef>();
  private rangeLimit = 65536;
  private finalized = false;
  private released = false;
  private disposed = false;
  private interrupted = false;
  private reading = false;
  private pump?: Promise<void>;
  private cutoff?: unknown;
  private stopped?: Promise<void>;
  constructor(private readonly bootstrap: InspectorBootstrap, private readonly hooks: TransportHooks, private readonly fetcher: typeof fetch = fetch) { this.origin = validateBootstrap(bootstrap); }
  async connect(): Promise<void> {
    if (this.pump) throw new Error('Observation already started');
    let resolveReady!: () => void; let rejectReady!: (error: Error) => void;
    const ready = new Promise<void>((resolve, reject) => { resolveReady = resolve; rejectReady = reject; });
    const timer = setTimeout(() => { rejectReady(new Error('Observation readiness timed out')); this.abort.abort(); }, 15_000);
    this.pump = this.consume(() => { clearTimeout(timer); resolveReady(); }).catch(() => {
      this.interrupted = true;
      clearTimeout(timer); rejectReady(new Error('Observation interrupted or incompatible'));
      if (!this.disposed) { this.hooks.issue('Observation interrupted; no automatic replay or execution outcome is implied'); this.hooks.state('disconnected'); }
    });
    await ready;
  }
  private async post(path: string, body: unknown, signal = this.abort.signal): Promise<Response> {
    return (0, this.fetcher)(new URL(path, this.origin), { method: 'POST', body: JSON.stringify(body), headers: { Authorization: `Bearer ${this.bootstrap.bearer}`, 'Content-Type': 'application/json' }, signal, credentials: 'omit', cache: 'no-store', redirect: 'error', referrerPolicy: 'no-referrer' });
  }
  private async control(path: string, body: unknown): Promise<unknown> {
    const timeout = new AbortController(); const timer = setTimeout(() => timeout.abort(), 15_000);
    const abort = () => timeout.abort(); this.abort.signal.addEventListener('abort', abort, { once: true });
    if (this.abort.signal.aborted) timeout.abort();
    try { return await boundedJson(await this.post(path, body, timeout.signal)); }
    finally { clearTimeout(timer); this.abort.signal.removeEventListener('abort', abort); }
  }
  private async consume(onReady: () => void): Promise<void> {
    const response = await this.post('/v1/xray/observe', { schema_version: 1, session_id: this.bootstrap.session_id, deep_capture: this.bootstrap.deep_capture });
    if (!response.ok || !response.body || !response.headers.get('content-type')?.toLowerCase().startsWith('text/event-stream')) throw new Error('Observation stream unavailable');
    const reader = response.body.getReader(); let queued: XrayEnvelope[] = [];
    const parser = new SseParser(data => { if (queued.length >= 128) throw new Error('Frame batch exceeds bound'); queued.push(envelope(JSON.parse(data))); });
    const flush = async () => { const batch = queued; queued = []; let becameReady = false; let becameFinal = false; for (const e of batch) {
      if (e.event.correlation.session_id !== this.bootstrap.session_id || (this.observation && this.observation !== e.observation_id)) throw new Error('Foreign observation evidence');
      if (!this.bootstrap.deep_capture && (e.event.sensitive || declaredReferences(e).length)) throw new Error('Sensitive content on shallow observation');
      this.tracker.accept(e);
      const first = !this.observation;
      if (first) {
        const d = e.event.data;
        if (e.event.domain !== 'xray' || e.event.name !== 'ready' || e.sequence !== 1 || !object(d) || d.schema_version !== 1 || d.deep_capture !== this.bootstrap.deep_capture || d.durable_replay !== false) throw new Error('Readiness mismatch');
        for (const key of ['queue_capacity', 'max_event_bytes', 'max_payload_bytes', 'max_retained_payload_bytes', 'max_retained_payloads', 'max_payload_range_bytes']) if (!uint(d[key]) || d[key] === 0) throw new Error('Invalid readiness bounds');
        this.rangeLimit = Math.min(65536, d.max_payload_range_bytes as number);
        this.observation = e.observation_id;
      } else if (e.event.domain === 'xray' && e.event.name === 'ready') throw new Error('Repeated readiness');
      for (const r of declaredReferences(e)) {
        const old = this.refs.get(r.payload_id); if (old && refKey(old) !== refKey(r)) throw new Error('Conflicting declared payload identity');
        if (!old && this.refs.size >= 2048) this.refs.delete(this.refs.keys().next().value!);
        this.refs.set(r.payload_id, r);
      }
      if (first) becameReady = true;
      if (e.event.domain === 'xray' && e.event.name === 'finalized') { this.finalized = true; this.cutoff = e.event.data; becameFinal = true; }
    }
      if (batch.length) await this.hooks.events(batch);
      if (becameReady) { this.hooks.state('live'); onReady(); }
      if (becameFinal) this.hooks.state('stopped');
    };
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) { parser.finish(); if (!this.finalized) throw new Error('EOF without cutoff receipt'); break; }
        // A single browser read can contain many frames; acknowledge small slices to bound worker traffic.
        for (let i = 0; i < value.length; i += 16_384) { parser.push(value.subarray(i, i + 16_384)); await flush(); }
        // The server may retain its SSE sender while the bounded payload lease lives.
        // The finalized frame is the delivery barrier; waiting for EOF before release deadlocks.
        if (this.finalized) { parser.finish(); break; }
      }
    } finally { await reader.cancel().catch(() => undefined); reader.releaseLock(); }
  }
  stop(): Promise<void> {
    if (this.released) return Promise.resolve();
    if (this.stopped) return this.stopped;
    this.stopped = this.finish().finally(() => { this.stopped = undefined; }); return this.stopped;
  }
  private async finish(): Promise<void> {
    if (!this.observation) { this.abort.abort(); return; }
    this.hooks.state('finalizing');
    const identity = { schema_version: 1, observation_id: this.observation };
    const receipt = await this.control('/v1/xray/finalize', identity);
    validateFinalization(receipt, this.observation);
    const timer = setTimeout(() => this.abort.abort(), 15_000);
    try { await this.pump; } finally { clearTimeout(timer); }
    if (!this.finalized || JSON.stringify(receipt) !== JSON.stringify(this.cutoff)) {
      // Compare fields, not object insertion order.
      if (!this.finalized || !object(receipt) || !object(this.cutoff) || ['observation_id', 'final_sequence', 'dropped_events', 'hub_contention_drops', 'evicted_payloads'].some(k => receipt[k] !== (this.cutoff as Record<string, unknown>)[k])) throw new Error('Cutoff delivery did not settle');
    }
    if (this.reading) throw new Error('Payload read still draining; retry release after completion');
    // Release response is an empty 204, not a JSON receipt.
    const releaseAbort = new AbortController(); const releaseTimer = setTimeout(() => releaseAbort.abort(), 15_000);
    try { const response = await this.post('/v1/xray/release', identity, releaseAbort.signal); if (!response.ok) throw new Error('Observation release pending'); }
    finally { clearTimeout(releaseTimer); }
    this.released = true; this.refs.clear(); this.abort.abort(); this.hooks.state('stopped');
  }
  canReadPayload(): boolean { return this.bootstrap.deep_capture && !!this.observation && !this.released && !this.disposed && !this.interrupted; }
  async readPayload(raw: unknown): Promise<Uint8Array> {
    const r = payloadRef(raw); const declared = this.refs.get(r.payload_id);
    if (!this.canReadPayload() || !declared || refKey(declared) !== refKey(r) || r.byte_len > MAX_PAYLOAD_BYTES || this.reading) throw new Error('Payload is not currently declared and authorized');
    this.reading = true;
    try {
      const bytes = new Uint8Array(r.byte_len); let offset = 0;
      do {
        const length = Math.min(this.rangeLimit, Math.max(1, r.byte_len - offset));
        const range = await this.control('/v1/xray/payload', { schema_version: 1, observation_id: this.observation, session_id: r.session_id, payload_id: r.payload_id, offset, length });
        if (!this.canReadPayload() || !object(range) || range.schema_version !== 1 || range.observation_id !== this.observation || refKey(payloadRef(range.payload)) !== refKey(r) || range.offset !== offset || !Array.isArray(range.bytes) || range.bytes.length > length || range.bytes.some(v => !uint(v) || v > 255) || typeof range.eof !== 'boolean') throw new Error('Invalid payload range');
        const end = offset + range.bytes.length;
        if (end > r.byte_len || range.eof !== (end === r.byte_len) || (!range.eof && end === offset)) throw new Error('Incomplete payload range');
        bytes.set(range.bytes, offset); offset = end;
        if (range.eof) break;
      } while (offset < r.byte_len);
      const digest = new Sha256();
      digest.update(bytes);
      if (digest.hex() !== r.sha256) throw new Error('Payload digest mismatch');
      if (!this.canReadPayload()) throw new Error('Payload lease ended during verification');
      return bytes;
    } finally { this.reading = false; }
  }
  dispose(): void { this.disposed = true; this.refs.clear(); this.abort.abort(); }
}
