import { describe, expect, it, vi } from 'vitest';
import type { XrayEnvelope } from '../contracts';
import { createRecordingReader, exportEvents, INSPECTOR_HEADER, isXrayEnvelope, openRecording, RECORDING_LIMITS } from './index';
import { createLiveRecording, pickLiveRecording, supportsLiveRecording, type WritableRecordingFile } from './live';
import { Sha256 } from './sha256';

const event = (sequence = 1): XrayEnvelope => ({ schema_version: 1, observation_id: 'obs', sequence, timestamp_unix_ms: 0, event: { domain: 'unknown-domain', name: 'sample', correlation: { session_id: 'session' }, data: { text: 'hello 世界' }, sensitive: true } });
const jsonFile = (text: string, path = 'events.jsonl') => {
  const file = new File([text], path.split('/').at(-1)!);
  if (path.includes('/')) Object.defineProperty(file, 'webkitRelativePath', { value: path });
  return file;
};
const sha = (data: string) => { const hash = new Sha256(); hash.update(new TextEncoder().encode(data)); return hash.hex(); };
function bundle(overrides: Record<string, unknown> = {}) {
  const cutoff = { observation_id: 'obs', final_sequence: 2, dropped_events: 0, hub_contention_drops: 0, evicted_payloads: 0 };
  const end: XrayEnvelope = { ...event(2), event: { ...event(2).event, domain: 'xray', name: 'finalized', data: cutoff } };
  const data = JSON.stringify(event()) + '\n' + JSON.stringify(end) + '\n';
  const payload = 'sensitive payload';
  const manifest = { schema_version: 1, envelope_schema_version: 1, observation_id: 'obs', session_id: 'session', state: 'finalized', sensitive: true, events: { relative_path: 'events.jsonl', byte_len: new TextEncoder().encode(data).length, sha256: sha(data), envelope_count: 2, last_sequence: 2 }, payloads: { id: { reference: { schema_version: 1, payload_id: 'id', session_id: 'session', content_type: 'text/plain', byte_len: payload.length, sha256: sha(payload) }, relative_path: 'payloads/data.bin' } }, missing_sequences: 0, omitted_events: 0, finalization: cutoff, evidence_complete: true, failure_code: null, ...overrides };
  return [jsonFile(JSON.stringify(manifest), 'bundle/manifest.json'), jsonFile(data, 'bundle/events.jsonl'), jsonFile(payload, 'bundle/payloads/data.bin')];
}
const header = JSON.stringify(INSPECTOR_HEADER) + '\n';

describe('bounded recording reader', () => {
  it('hashes known SHA-256 vectors across arbitrary chunk boundaries', () => {
    expect(sha('')).toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
    expect(sha('abc')).toBe('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
    const hash = new Sha256();
    for (let i = 0; i < 1000; i++) hash.update(new TextEncoder().encode('a'.repeat(1000)));
    expect(hash.hex()).toBe('cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0');
  });
  it('round trips an explicitly incomplete Unicode inspector export', async () => {
    const blob = exportEvents([event()]);
    const text = await blob.text();
    expect(JSON.parse(text.split('\n')[0])).toEqual(INSPECTOR_HEADER);
    const result = await openRecording([jsonFile(text)]);
    expect(result.events).toEqual([event()]);
    expect(result.issues.some((issue) => issue.kind === 'incomplete_recording')).toBe(true);
  });
  it('supports v1 and known v2 bundles and only explicitly reads verified payload bytes', async () => {
    for (const overrides of [{}, { schema_version: 2, segment: { kind: 'initial' } }]) {
      const files = bundle(overrides);
      Object.defineProperty(files[2], 'arrayBuffer', { value: async () => { throw new Error('not requested'); } });
      const reader = await createRecordingReader(files);
      expect((await reader.search('世界')).events).toHaveLength(1);
      expect(reader.issues.some((issue) => issue.kind === 'event_integrity_pending')).toBe(false);
      expect(reader.issues.some((issue) => ['event_integrity_failed', 'missing_cutoff', 'cutoff_mismatch'].includes(issue.kind))).toBe(false);
      await expect(reader.readPayload('id')).rejects.toThrow('Payload file read failed');
    }
    const reader = await createRecordingReader(bundle());
    expect(new TextDecoder().decode(await reader.readPayload('id'))).toBe('sensitive payload');
  });
  it('rejects traversal, duplicate relative paths, ambiguous roots, versions, and missing files', async () => {
    await expect(createRecordingReader([jsonFile('', '../events.jsonl')])).rejects.toThrow();
    await expect(createRecordingReader([jsonFile(''), jsonFile('')])).rejects.toThrow();
    await expect(createRecordingReader([...bundle(), jsonFile('{}', 'other/manifest.json')])).rejects.toThrow();
    await expect(createRecordingReader([...bundle(), jsonFile('', 'other/extra')])).rejects.toThrow();
    await expect(createRecordingReader(bundle({ schema_version: 99 }))).rejects.toThrow();
    await expect(createRecordingReader(bundle().filter((file) => file.name !== 'events.jsonl'))).rejects.toThrow();
    const files = bundle();
    const manifest = JSON.parse(await files[0].text());
    manifest.payloads.id.relative_path = '../outside';
    files[0] = jsonFile(JSON.stringify(manifest), 'bundle/manifest.json');
    await expect(createRecordingReader(files)).rejects.toThrow();
  });
  it('surfaces integrity mismatches and refuses corrupted payload access', async () => {
    const files = bundle();
    files[1] = jsonFile(JSON.stringify(event(2)) + '\n', 'bundle/events.jsonl');
    files[2] = jsonFile('x'.repeat('sensitive payload'.length), 'bundle/payloads/data.bin');
    const reader = await createRecordingReader(files);
    await reader.search('');
    expect(reader.issues.some((issue) => issue.kind === 'event_integrity_failed')).toBe(true);
    await expect(reader.readPayload('id')).rejects.toThrow('integrity');
  });
  it('preserves unknown domains but rejects unsupported versions and unsafe u64s', () => {
    expect(isXrayEnvelope(event())).toBe(true);
    expect(isXrayEnvelope({ ...event(), schema_version: 2 })).toBe(false);
    expect(isXrayEnvelope({ ...event(), sequence: 2 ** 53 })).toBe(false);
    expect(isXrayEnvelope({ ...event(), event: { ...event().event, sensitive: undefined } })).toBe(false);
  });
  it('bounds line assembly and catches invalid UTF-8 without returning invented evidence', async () => {
    const long = await openRecording([jsonFile(header + 'x'.repeat(RECORDING_LIMITS.lineBytes + 1))]);
    expect(long.events).toEqual([]);
    expect(long.issues.some((issue) => issue.kind === 'read_failed')).toBe(true);
    const bad = new File([header, new Uint8Array([255])], 'bad.jsonl');
    expect((await openRecording([bad])).issues.some((issue) => issue.kind === 'read_failed')).toBe(true);
  });
  it('keeps large-file browsing and search bounded with truthful partial results', async () => {
    const file = jsonFile(header + Array.from({ length: RECORDING_LIMITS.retainedEvents + 2 }, (_, i) => JSON.stringify(event(i + 1)) + '\n').join(''));
    Object.defineProperty(file, 'arrayBuffer', { value: () => { throw new Error('full read forbidden'); } });
    const opened = await openRecording([file]);
    expect(opened.events).toHaveLength(RECORDING_LIMITS.retainedEvents);
    expect(opened.issues.some((issue) => issue.kind === 'retained_window')).toBe(true);
    const reader = await createRecordingReader([file]);
    const result = await reader.search('hello', { offset: 2, limit: 3, maxScanned: 10 });
    expect(result.events.map((item) => item.sequence)).toEqual([3, 4, 5]);
    expect(result.hasMore).toBe(true);
    expect(result.scanned).toBeLessThanOrEqual(10);
    const fieldResult = await reader.search('domain:unknown-domain -name:finalized', { limit: 1 });
    expect(fieldResult.events.map((item) => item.sequence)).toEqual([1]);
    await expect(reader.search('unsupported:value')).rejects.toThrow();
    const controller = new AbortController(); controller.abort();
    await expect(reader.search('', { signal: controller.signal })).rejects.toBeDefined();
  });
  it('handles explicit gap ranges without inventing a second discontinuity', async () => {
    const gap: XrayEnvelope = { ...event(1), event: { ...event().event, domain: 'xray', name: 'gap', data: { first_sequence: 1, last_sequence: 3, reason: 'overflow' } } };
    const result = await openRecording([jsonFile(header + JSON.stringify(gap) + '\n' + JSON.stringify(event(4)) + '\n')]);
    expect(result.events.map((item) => item.sequence)).toEqual([1, 4]);
    expect(result.issues.some((issue) => issue.kind === 'sequence_gap')).toBe(false);
    expect(result.issues.some((issue) => issue.kind === 'capture_loss')).toBe(true);
  });
  it('respects chunk-split UTF-8 and reports typed capture declarations it cannot interpret', async () => {
    const value = event();
    value.event.data = 'x'.repeat(65_200) + '界😀'.repeat(1000);
    const result = await openRecording([jsonFile(header + JSON.stringify(value) + '\n')]);
    expect(result.events).toEqual([value]);
    expect(isXrayEnvelope({ ...event(), event: { ...event().event, capture: { schema_version: 99, attachments: [] } } })).toBe(false);
  });
  it('keeps payload-looking ordinary JSON opaque', async () => {
    const value = event();
    value.event.data = { payload_id: 'not-authority', sha256: 'x', relative_path: '../no' };
    const result = await openRecording([jsonFile(header + JSON.stringify(value) + '\n')]);
    expect(result.events).toEqual([value]);
    const reader = await createRecordingReader([jsonFile(header)]);
    await expect(reader.readPayload('not-authority')).rejects.toThrow('not locally retained');
    await expect(reader.readPayload('__proto__')).rejects.toThrow('not locally retained');
  });
  it('reports headerless, future-header, malformed, and interrupted evidence explicitly', async () => {
    for (const content of ['', JSON.stringify(event()), JSON.stringify({ ...INSPECTOR_HEADER, schema_version: 2 })]) {
      const result = await openRecording([jsonFile(content)]);
      expect(result.events).toHaveLength(0);
      expect(result.issues.some((issue) => issue.kind === 'read_failed')).toBe(true);
    }
    const opened = await openRecording([jsonFile(header + '{bad}\n' + JSON.stringify(event(3)))]);
    expect(opened.events).toHaveLength(1);
    expect(opened.issues.map((issue) => issue.kind)).toContain('invalid_line');
    expect((await openRecording(bundle({ state: 'recording', evidence_complete: false }))).issues.map((issue) => issue.kind)).toContain('incomplete_recording');
  });
});

function deferred() { let resolve!: () => void; const promise = new Promise<void>((done) => { resolve = done; }); return { promise, resolve }; }
describe('explicit bounded disk recording', () => {
  it('requires both browser support and an active gesture before opening a file', async () => {
    const picker = vi.fn(async () => ({ async createWritable() { return { async write() {}, async close() {}, async abort() {} }; } }));
    vi.stubGlobal('isSecureContext', true);
    vi.stubGlobal('showSaveFilePicker', picker);
    vi.stubGlobal('navigator', { userActivation: { isActive: false } });
    try {
      expect(supportsLiveRecording()).toBe(true);
      await expect(pickLiveRecording()).rejects.toThrow('user gesture');
      expect(picker).not.toHaveBeenCalled();
      vi.stubGlobal('navigator', { userActivation: { isActive: true } });
      const sink = await pickLiveRecording();
      expect(picker).toHaveBeenCalledOnce();
      await sink.close();
    } finally { vi.unstubAllGlobals(); }
  });
  it('aborts a blocked stream explicitly without waiting for its pending write', async () => {
    const gate = deferred(); let writes = 0, aborted = false;
    const sink = await createLiveRecording({ async write() { if (++writes > 1) await gate.promise; }, async close() {}, async abort() { aborted = true; } });
    sink.append(event());
    expect((await sink.abort()).state).toBe('aborted');
    expect(aborted).toBe(true);
    expect(sink.append(event())).toBe(false);
    gate.resolve();
  });
  it('flushes incrementally and closes once without claiming full evidence', async () => {
    const written: string[] = []; let closed = 0;
    const writer: WritableRecordingFile = { async write(line) { written.push(line); }, async close() { closed++; }, async abort() {} };
    const sink = await createLiveRecording(writer);
    expect(sink.append(event())).toBe(true);
    const result = await sink.close();
    expect(result.state).toBe('closed'); expect(result.writtenEvents).toBe(1);
    await sink.close(); expect(closed).toBe(1);
    expect(sink.append(event(2))).toBe(false);
    expect(JSON.parse(written[0])).toEqual(INSPECTOR_HEADER);
  });
  it('fails visibly on backpressure without accumulating beyond the queue bound', async () => {
    const gate = deferred(); let writes = 0, aborts = 0;
    const writer: WritableRecordingFile = { async write() { if (++writes > 1) await gate.promise; }, async close() {}, async abort() { aborts++; } };
    const sink = await createLiveRecording(writer);
    for (let i = 1; i <= 128; i++) expect(sink.append(event(i))).toBe(true);
    expect(sink.append(event(129))).toBe(false);
    expect(sink.status().state).toBe('failed'); expect(sink.status().queuedBytes).toBe(0);
    gate.resolve();
    expect((await sink.close()).state).toBe('failed'); expect(aborts).toBe(1);
  });
  it('normalizes write and close failures without leaking raw errors', async () => {
    let writes = 0;
    const sink = await createLiveRecording({ async write() { if (++writes > 1) throw new Error('secret/path'); }, async close() {}, async abort() {} });
    sink.append(event());
    expect((await sink.close()).failure).not.toContain('secret');
    const closing = await createLiveRecording({ async write() {}, async close() { throw new Error('secret'); }, async abort() {} });
    expect((await closing.close()).state).toBe('failed');
  });
});
