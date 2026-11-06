import { describe, expect, it } from 'vitest';
import type { XrayEnvelope } from '../contracts';
import { declaredReferences, envelope, SequenceTracker, SseParser } from './protocol';
import { EvidenceStore, InspectorWorkerEngine } from './store';
import { parseQuery } from './search';
export const event = (sequence: number, name = 'delta', data: unknown = {}): XrayEnvelope => ({ schema_version: 1, observation_id: 'o', sequence, timestamp_unix_ms: 0, event: { domain: name === 'gap' || name === 'finalized' ? 'xray' : 'future-domain', name, correlation: { session_id: 's', tool_call_id: 'tool-1' }, sensitive: false, data } });
describe('strict bounded observation protocol', () => {
  it('opens an incomplete exported window without inventing its missing prefix', () => {
    const store = new EvidenceStore(); store.reset('opened-recording');
    store.ingest(event(91)); store.ingest(event(92));
    expect(store.snapshot().events.map(e => e.sequence)).toEqual([91, 92]);
    expect(store.snapshot().issues).toContainEqual(expect.objectContaining({ kind: 'recording-prefix-missing' }));
    const live = new EvidenceStore(); expect(() => live.ingest(event(91))).toThrow();
  });
  it('preserves unknown event names but rejects unsupported versions and structures', () => {
    expect(envelope(event(1)).event.domain).toBe('future-domain');
    for (const value of [{ ...event(1), schema_version: 2 }, { ...event(1), sequence: 1.5 }, { ...event(1), event: {} }]) expect(() => envelope(value)).toThrow();
  });
  it('tracks inclusive gaps and refuses changed identity and post-cutoff events', () => {
    const tracker = new SequenceTracker(); tracker.accept(event(1)); tracker.accept(event(2, 'gap', { first_sequence: 2, last_sequence: 5, reason: 'overflow' }));
    expect(() => tracker.accept(event(3))).toThrow(); tracker.accept(event(6));
    expect(() => tracker.accept({ ...event(7), observation_id: 'foreign' })).toThrow();
    const done = new SequenceTracker(); done.accept(event(1, 'finalized', { observation_id: 'o', final_sequence: 1, dropped_events: 0, hub_contention_drops: 1, evicted_payloads: 0 })); expect(() => done.accept(event(2))).toThrow();
  });
  it('parses all byte splits, Unicode, CRLF, comments and multiline data', () => {
    const wire = new TextEncoder().encode(': heartbeat\r\n\r\nevent: xray\r\ndata: {"text":\r\ndata: "😀漢字"}\r\n\r\n');
    for (let split = 0; split <= wire.length; split++) { const result: string[] = []; const p = new SseParser(s => result.push(s)); p.push(wire.subarray(0, split)); p.push(wire.subarray(split)); p.finish(); expect(JSON.parse(result[0])).toEqual({ text: '😀漢字' }); }
  });
  it('rejects malformed UTF8, oversized bytes, partial frames, transport failures and replay ids', () => {
    expect(() => new SseParser(() => {}).push(new Uint8Array([100, 97, 116, 97, 58, 255, 10, 10]))).toThrow();
    expect(() => new SseParser(() => {}, 8).push(new TextEncoder().encode('data: 漢字'))).toThrow();
    const p = new SseParser(() => {}); p.push(new TextEncoder().encode('data: x')); expect(() => p.finish()).toThrow();
    for (const text of ['event: error\ndata: {}\n\n', 'id: 1\nevent: xray\ndata: {}\n\n']) expect(() => new SseParser(() => {}).push(new TextEncoder().encode(text))).toThrow();
  });
  it('authorizes only explicit typed capture locations with exact session and metadata', () => {
    const ref = { schema_version: 1, payload_id: 'p', session_id: 's', content_type: 'text/plain', byte_len: 0, sha256: '0'.repeat(64) };
    const e = event(1, 'capture', { arbitrary: ref, 'a/b': ref }); expect(declaredReferences(e)).toEqual([]);
    e.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/a~1b' }] }; expect(declaredReferences(e)).toEqual([ref]);
    e.event.capture = { schema_version: 2, attachments: [] }; expect(() => declaredReferences(e)).toThrow();
    e.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/arbitrary' }, { kind: 'payload_ref', pointer: '/arbitrary' }] }; expect(() => declaredReferences(e)).toThrow();
  });
});
describe('bounded indexed store and worker', () => {
  it('evicts count and index bytes separately from server loss and searches only retention', () => {
    const store = new EvidenceStore(2); store.ingest(event(1)); store.ingest(event(2)); store.ingest(event(3));
    expect(store.snapshot()).toMatchObject({ retained: 2, received: 3, evicted: 1, matches: 2 });
    expect(store.snapshot().issues[0].kind).toBe('browser-eviction');
    const tiny = new EvidenceStore(10, 1); tiny.ingest(event(1)); expect(tiny.snapshot()).toMatchObject({ retained: 0, retainedBytes: 0, evicted: 1 });
  });
  it('supports quoted field terms, negation, query errors, and capped windows', () => {
    const store = new EvidenceStore(); store.ingest(event(1, 'alpha', { message: 'hello world' })); store.ingest(event(2, 'beta'));
    store.query('data:"hello world" tool:tool-1 -name:beta'); expect(store.snapshot().events.map(e => e.sequence)).toEqual([1]);
    store.query('"unclosed'); expect(store.snapshot().queryError).not.toBeNull(); expect(store.snapshot().matches).toBe(1);
    expect(() => parseQuery('unknown:field')).toThrow(); store.query(''); store.window(1, 9000); expect(store.snapshot().events.map(e => e.sequence)).toEqual([2]);
    for (let n = 3; n < 2100; n++) store.ingest(event(n)); store.window(0, 99999); expect(store.snapshot().events.length).toBe(2000);
  });
  it('isolates epochs and resets observations when opening a recording', () => {
    const engine = new InspectorWorkerEngine(); engine.handle({ epoch: 1, id: 1, kind: 'reset', scope: 'retained-live' }); engine.handle({ epoch: 1, id: 2, kind: 'ingest', events: [event(1)] });
    engine.handle({ epoch: 2, id: 3, kind: 'reset', scope: 'opened-recording' }); expect(engine.handle({ epoch: 1, id: 4, kind: 'ingest', events: [event(2)] }).snapshot).toBeUndefined();
    expect(engine.handle({ epoch: 2, id: 5, kind: 'state', state: 'recording' }).snapshot).toMatchObject({ retained: 0, scope: 'opened-recording' });
    expect(engine.handle({ epoch: 2, id: 6, kind: 'ingest', events: [event(2)] }).snapshot?.events.map(e => e.sequence)).toEqual([2]);
    expect(engine.handle({ epoch: 2, id: 7, kind: 'ingest', events: [event(4)] }).error).toBeDefined();
  });
});
