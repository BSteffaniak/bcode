import type { XrayEnvelope } from '../contracts';

export const MAX_FRAME_BYTES = 2 * 1024 * 1024;
export const MAX_PAYLOAD_BYTES = 1024 * 1024;
export const object = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);
export const uint = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 0;
const text = (v: unknown): v is string => typeof v === 'string' && v.length > 0 && v.length <= 4096;
const exact = (v: Record<string, unknown>, keys: string[]) => Object.keys(v).every(k => keys.includes(k));
export interface PayloadRef { schema_version: 1; payload_id: string; session_id: string; content_type: string; byte_len: number; sha256: string }
export function payloadRef(v: unknown): PayloadRef {
  if (!object(v) || !exact(v, ['schema_version', 'payload_id', 'session_id', 'content_type', 'byte_len', 'sha256']) || v.schema_version !== 1 || !text(v.payload_id) || !text(v.session_id) || !text(v.content_type) || !uint(v.byte_len) || typeof v.sha256 !== 'string' || !/^[a-f0-9]{64}$/.test(v.sha256)) throw new Error('Invalid typed payload reference');
  return v as unknown as PayloadRef;
}
export const refKey = (r: PayloadRef): string => JSON.stringify([r.schema_version, r.payload_id, r.session_id, r.content_type, r.byte_len, r.sha256]);
export function declaredReferences(e: XrayEnvelope): PayloadRef[] {
  const c = e.event.capture;
  if (c === undefined || c === null) return [];
  if (!object(c) || !exact(c, ['schema_version', 'attachments']) || c.schema_version !== 1 || !Array.isArray(c.attachments) || c.attachments.length > 1024) throw new Error('Unsupported capture declaration');
  const pointers = new Set<string>();
  const identities = new Map<string, string>();
  return c.attachments.map((a: unknown) => {
    if (!object(a) || !exact(a, ['kind', 'pointer']) || a.kind !== 'payload_ref' || typeof a.pointer !== 'string' || a.pointer.length > 4096 || pointers.has(a.pointer)) throw new Error('Invalid capture location');
    pointers.add(a.pointer);
    let v: unknown = e.event.data;
    if (a.pointer !== '') {
      if (!a.pointer.startsWith('/')) throw new Error('Invalid capture pointer');
      for (const part of a.pointer.slice(1).split('/')) {
        if (/~(?:[^01]|$)/.test(part)) throw new Error('Invalid capture pointer');
        const key = part.replace(/~1/g, '/').replace(/~0/g, '~');
        if ((!object(v) && !Array.isArray(v)) || !Object.hasOwn(v, key) || (Array.isArray(v) && !/^(0|[1-9][0-9]*)$/.test(key))) throw new Error('Missing capture location');
        v = (v as Record<string, unknown>)[key];
      }
    }
    const r = payloadRef(v);
    if (r.session_id !== e.event.correlation.session_id) throw new Error('Foreign payload reference');
    const prior = identities.get(r.payload_id);
    if (prior && prior !== refKey(r)) throw new Error('Conflicting payload reference');
    identities.set(r.payload_id, refKey(r));
    return r;
  });
}
export function envelope(v: unknown): XrayEnvelope {
  if (!object(v) || v.schema_version !== 1 || !text(v.observation_id) || !uint(v.sequence) || v.sequence < 1 || v.sequence === Number.MAX_SAFE_INTEGER || !uint(v.timestamp_unix_ms) || !object(v.event)) throw new Error('Unsupported X-ray envelope');
  const e = v.event;
  if (!text(e.domain) || !text(e.name) || typeof e.sensitive !== 'boolean' || !Object.hasOwn(e, 'data') || !object(e.correlation) || !text(e.correlation.session_id)) throw new Error('Malformed X-ray event');
  for (const k of ['turn_id', 'request_id', 'tool_call_id', 'run_id', 'activation_id', 'attempt_id']) if (e.correlation[k] !== undefined && e.correlation[k] !== null && !text(e.correlation[k])) throw new Error('Malformed correlation');
  const result = v as unknown as XrayEnvelope;
  declaredReferences(result);
  return result;
}
export class SequenceTracker {
  private observation: string | undefined;
  constructor(private next = 1) {}
  private ended = false;
  accept(e: XrayEnvelope): void {
    if (this.ended || (this.observation && this.observation !== e.observation_id) || e.sequence !== this.next) throw new Error('Observation identity or sequence discontinuity');
    this.observation = e.observation_id;
    this.next = e.sequence + 1;
    if (e.event.domain !== 'xray') return;
    const d = e.event.data;
    if (e.event.name === 'gap') {
      if (!object(d) || d.first_sequence !== e.sequence || !uint(d.last_sequence) || d.last_sequence < e.sequence || d.last_sequence >= Number.MAX_SAFE_INTEGER || !text(d.reason)) throw new Error('Invalid server gap');
      this.next = d.last_sequence + 1;
    }
    if (e.event.name === 'finalized') { validateFinalization(d, e.observation_id, e.sequence); this.ended = true; }
  }
}
export function validateFinalization(v: unknown, id: string, sequence?: number): void {
  if (!object(v) || v.observation_id !== id || !uint(v.final_sequence) || (sequence !== undefined && v.final_sequence !== sequence) || !uint(v.dropped_events) || !uint(v.hub_contention_drops) || !uint(v.evicted_payloads)) throw new Error('Invalid cutoff receipt');
}

/** Incremental fatal UTF-8 SSE parser; frame bounds count wire bytes, not UTF-16 characters. */
export class SseParser {
  private bytes: number[] = [];
  private line: number[] = [];
  private frameBytes = 0;
  private pendingCR = false;
  constructor(private readonly onData: (data: string) => void, private readonly maxBytes = MAX_FRAME_BYTES) {}
  push(chunk: Uint8Array): void {
    for (const byte of chunk) {
      if (this.pendingCR) { this.pendingCR = false; if (byte === 10) continue; }
      this.frameBytes++;
      if (this.frameBytes > this.maxBytes) throw new Error('SSE frame exceeds byte bound');
      if (byte === 10 || byte === 13) { this.endLine(); this.pendingCR = byte === 13; }
      else this.line.push(byte);
    }
  }
  finish(): void { if (this.line.length || this.bytes.length) throw new Error('Truncated SSE frame'); }
  private endLine(): void {
    if (this.line.length) { for (const byte of this.line) this.bytes.push(byte); this.bytes.push(10); this.line = []; return; }
    const frame = new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array(this.bytes));
    this.bytes = []; this.frameBytes = 0;
    let event = 'message'; const data: string[] = [];
    for (const line of frame.split('\n')) {
      if (!line || line.startsWith(':')) continue;
      const colon = line.indexOf(':'); const key = colon < 0 ? line : line.slice(0, colon);
      let value = colon < 0 ? '' : line.slice(colon + 1); if (value.startsWith(' ')) value = value.slice(1);
      if (key === 'event') event = value;
      if (key === 'data') data.push(value);
      if (key === 'id' || key === 'retry') throw new Error('Replay metadata is unsupported');
    }
    if (data.length) { if (event !== 'xray') throw new Error('Non-envelope transport event'); this.onData(data.join('\n')); }
  }
}
