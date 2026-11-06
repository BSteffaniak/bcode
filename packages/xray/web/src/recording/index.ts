import type { EvidenceIssue, XrayEnvelope } from '../contracts';
import { Sha256 } from './sha256';
import { declaredReferences, refKey, validateFinalization } from '../core/protocol';
import { matchesQuery, parseQuery, searchDocument } from '../core/search';
export { createLiveRecording, pickLiveRecording, supportsLiveRecording } from './live';
export type { LiveRecordingSink, RecordingStatus, WritableRecordingFile } from './live';

export const RECORDING_LIMITS = Object.freeze({ files: 8192, totalBytes: 1024 ** 3, manifestBytes: 4 * 1024 ** 2, lineBytes: 1024 ** 2, payloadBytes: 16 * 1024 ** 2, retainedEvents: 10_000, retainedBytes: 16 * 1024 ** 2, issues: 128, chunkBytes: 64 * 1024 });
export const INSPECTOR_HEADER = Object.freeze({ format: 'bcode-xray-inspector', schema_version: 1, coverage: 'incomplete_retained_window', payloads: 'not_included', sensitive: true });
type ObjectValue = Record<string, unknown>;
const object = (value: unknown): value is ObjectValue => typeof value === 'object' && value !== null && !Array.isArray(value);
const integer = (value: unknown): value is number => typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
const text = (value: unknown): value is string => typeof value === 'string' && value.length > 0 && value.length <= 4096;
const digest = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
const exactKeys = (value: ObjectValue, keys: string[]) => Object.keys(value).every((key) => keys.includes(key));
const encoder = new TextEncoder();

/** Unknown domains remain data; unsafe u64 values and unsupported envelopes are rejected. */
export function isXrayEnvelope(value: unknown): value is XrayEnvelope {
  if (!object(value) || value.schema_version !== 1 || !text(value.observation_id) || !integer(value.sequence) || value.sequence < 1 || value.sequence >= Number.MAX_SAFE_INTEGER || !integer(value.timestamp_unix_ms) || !object(value.event)) return false;
  const event = value.event;
  if (!text(event.domain) || !text(event.name) || typeof event.sensitive !== 'boolean' || !('data' in event) || !object(event.correlation) || !text(event.correlation.session_id)) return false;
  const valid = ['turn_id', 'request_id', 'tool_call_id', 'run_id', 'activation_id', 'attempt_id'].every((key) => event.correlation && object(event.correlation) && (event.correlation[key] == null || text(event.correlation[key])));
  if (!valid) return false;
  try {
    declaredReferences(value as unknown as XrayEnvelope);
    JSON.stringify(value);
    return true;
  } catch { return false; }
}

export function encodeEvent(event: XrayEnvelope): string {
  if (!isXrayEnvelope(event)) throw new Error('Unsupported X-ray envelope');
  let line: string;
  try { line = JSON.stringify(event) + '\n'; } catch { throw new Error('Envelope is not serializable diagnostic data'); }
  if (encoder.encode(line).length > RECORDING_LIMITS.lineBytes) throw new Error('Recording line limit exceeded');
  return line;
}

/** Explicit sensitive export of a bounded retained window, never a complete bundle. */
export function exportEvents(events: XrayEnvelope[]): Blob {
  if (events.length > RECORDING_LIMITS.retainedEvents) throw new Error('Export event limit exceeded');
  const parts = [JSON.stringify(INSPECTOR_HEADER) + '\n'];
  let bytes = encoder.encode(parts[0]).length;
  for (const event of events) {
    const line = encodeEvent(event);
    bytes += encoder.encode(line).length;
    if (bytes > RECORDING_LIMITS.retainedBytes) throw new Error('Export byte limit exceeded');
    parts.push(line);
  }
  return new Blob(parts, { type: 'application/x-ndjson' });
}

interface Payload { reference: { schema_version: 1; payload_id: string; session_id: string; content_type: string; byte_len: number; sha256: string }; relative_path: string | null }
interface Manifest { schema_version: 1 | 2; envelope_schema_version: 1; observation_id: string; session_id: string; state: string; events: { relative_path: string; byte_len: number; sha256: string; envelope_count: number; last_sequence: number }; payloads: Record<string, Payload>; missing_sequences: number; omitted_events: number; evidence_complete: boolean; segment?: unknown }

function confined(path: string): boolean {
  return path.length <= 4096 && !/[\\\u0000-\u001f\u007f:%?#]/.test(path) && path.split('/').every((part) => part !== '' && part !== '.' && part !== '..');
}
function manifestValue(value: unknown): Manifest {
  if (!object(value) || ![1, 2].includes(value.schema_version as number) || value.envelope_schema_version !== 1 || !text(value.observation_id) || !text(value.session_id) || !['recording', 'finalized', 'interrupted', 'failed'].includes(value.state as string) || value.sensitive !== true || typeof value.evidence_complete !== 'boolean' || !integer(value.missing_sequences) || !integer(value.omitted_events) || !object(value.events) || !object(value.payloads)) throw new Error('Unsupported recording manifest');
  if (!exactKeys(value, ['schema_version', 'envelope_schema_version', 'observation_id', 'session_id', 'state', 'sensitive', 'events', 'payloads', 'missing_sequences', 'omitted_events', 'finalization', 'evidence_complete', 'failure_code', 'segment'])) throw new Error('Unsupported manifest fields');
  if (!('finalization' in value) || !('failure_code' in value) || (value.failure_code !== null && !text(value.failure_code))) throw new Error('Invalid lifecycle metadata');
  if (value.finalization !== null) validateFinalization(value.finalization, value.observation_id);
  if (value.state === 'finalized' && value.finalization === null) throw new Error('Finalized recording has no cutoff receipt');
  if (value.evidence_complete && (value.state !== 'finalized' || value.missing_sequences !== 0 || value.omitted_events !== 0 || value.failure_code !== null)) throw new Error('Conflicting completeness claim');
  const events = value.events;
  if (!exactKeys(events, ['relative_path', 'byte_len', 'sha256', 'envelope_count', 'last_sequence'])) throw new Error('Unsupported event metadata');
  if (events.relative_path !== 'events.jsonl' || !integer(events.byte_len) || !digest(events.sha256) || !integer(events.envelope_count) || !integer(events.last_sequence)) throw new Error('Invalid event integrity metadata');
  if (value.schema_version === 2) {
    if (!object(value.segment) || (value.segment.kind !== 'initial' && !(value.segment.kind === 'restart_after_gap' && text(value.segment.previous_observation_id)))) throw new Error('Unsupported capture segment');
  } else if ('segment' in value) throw new Error('Unsupported v1 capture segment');
  if (value.schema_version === 2 && object(value.segment) && value.segment.kind === 'restart_after_gap' && value.segment.previous_observation_id === value.observation_id) throw new Error('Invalid segment identity');
  if (Object.keys(value.payloads).length > 4096) throw new Error('Payload count limit exceeded');
  const paths = new Set([events.relative_path]);
  for (const [id, payload] of Object.entries(value.payloads)) {
    if (!object(payload) || !exactKeys(payload, ['reference', 'relative_path']) || !object(payload.reference) || !exactKeys(payload.reference, ['schema_version', 'payload_id', 'session_id', 'content_type', 'byte_len', 'sha256'])) throw new Error('Invalid payload metadata');
    const ref = payload.reference;
    if (!text(id) || ref.schema_version !== 1 || ref.payload_id !== id || ref.session_id !== value.session_id || !text(ref.content_type) || !integer(ref.byte_len) || !digest(ref.sha256)) throw new Error('Unsupported payload reference');
    if (payload.relative_path !== null) {
      if (!text(payload.relative_path) || !confined(payload.relative_path) || paths.has(payload.relative_path) || !payload.relative_path.startsWith('payloads/')) throw new Error('Ambiguous payload path');
      paths.add(payload.relative_path);
    }
  }
  return value as unknown as Manifest;
}

export interface RecordingSearchOptions { offset?: number; limit?: number; maxScanned?: number; signal?: AbortSignal }
export interface RecordingSearchResult { events: XrayEnvelope[]; scanned: number; hasMore: boolean; issues: EvidenceIssue[] }
export interface RecordingReader {
  /** Shared bounded issue list. Events yielded before EOF have not yet passed event-file digest verification. */
  readonly issues: EvidenceIssue[];
  events(signal?: AbortSignal): AsyncGenerator<XrayEnvelope>;
  search(query: string, options?: RecordingSearchOptions): Promise<RecordingSearchResult>;
  /** Reads only explicitly selected local files; verifies all bytes before returning. */
  readPayload(id: string): Promise<Uint8Array>;
}

/** File selections are snapshots, not OS paths. Browser APIs cannot attest symlink ancestry. */
export async function createRecordingReader(files: File[]): Promise<RecordingReader> {
  const issues: EvidenceIssue[] = [];
  const seenIssues = new Set<string>();
  const issue = (kind: string, message: string) => {
    const key = kind + ':' + message;
    if (seenIssues.has(key)) return;
    if (issues.length >= RECORDING_LIMITS.issues) return;
    seenIssues.add(key);
    issues.push(issues.length === RECORDING_LIMITS.issues - 1 ? { kind: 'issue_limit', message: 'Additional recording issues are not retained.' } : { kind, message });
  };
  if (!files.length || files.length > RECORDING_LIMITS.files || files.reduce((sum, file) => sum + file.size, 0) > RECORDING_LIMITS.totalBytes) throw new Error('Recording selection limit exceeded');
  const selected = new Map<string, File>();
  for (const file of files) {
    const path = file.webkitRelativePath || file.name;
    if (!confined(path) || selected.has(path)) throw new Error('Unsafe or duplicate selected path');
    selected.set(path, file);
  }
  const manifests = [...selected.keys()].filter((path) => path === 'manifest.json' || path.endsWith('/manifest.json'));
  let manifest: Manifest | undefined;
  let eventFile: File;
  const relative = new Map<string, File>();
  if (manifests.length) {
    if (manifests.length !== 1) throw new Error('Ambiguous bundle root');
    const manifestPath = manifests[0], root = manifestPath.slice(0, -'manifest.json'.length);
    for (const [path, file] of selected) {
      if (!path.startsWith(root)) throw new Error('Selection contains multiple roots');
      relative.set(path.slice(root.length), file);
    }
    const file = selected.get(manifestPath)!;
    if (file.size > RECORDING_LIMITS.manifestBytes) throw new Error('Manifest size limit exceeded');
    let manifestJson: unknown;
    try { manifestJson = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(await file.arrayBuffer())); }
    catch { throw new Error('Recording manifest could not be read or parsed'); }
    manifest = manifestValue(manifestJson);
    const found = relative.get(manifest.events.relative_path);
    if (!found) throw new Error('Declared event file is missing');
    eventFile = found;
    issue('bundle_scope', 'Local diagnostic bundle only: manifest claims and lifecycle receipts are not completeness certifications. Browser reader does not certify full-session coverage or execution outcome. Payloads are verified only when explicitly opened.');
    issue('browser_path_scope', 'Browser-selected files are exact relative snapshots; symlink ancestry cannot be attested.');
    issue('event_integrity_pending', 'Event integrity is checked only after a complete streaming scan; partial browsing/search is unverified.');
    if (manifest.state !== 'finalized' || !manifest.evidence_complete || manifest.missing_sequences || manifest.omitted_events || (object(manifest.segment) && manifest.segment.kind === 'restart_after_gap')) issue('incomplete_recording', 'Manifest reports incomplete or interrupted diagnostic evidence; no cause of interruption is inferred.');
    for (const payload of Object.values(manifest.payloads)) {
      if (payload.relative_path === null || !relative.has(payload.relative_path)) issue('missing_payload', 'One or more declared payload files are unavailable.');
    }
  } else {
    if (files.length !== 1) throw new Error('Select one inspector JSONL export or an exact bundle directory');
    eventFile = files[0];
    issue('incomplete_recording', 'JSONL is retained-window evidence without bundle payloads, integrity certification, or full-session coverage.');
  }

  async function* events(signal?: AbortSignal): AsyncGenerator<XrayEnvelope> {
    if (manifest && !issues.some((entry) => entry.kind === 'event_integrity_pending')) {
      issues.push({ kind: 'event_integrity_pending', message: 'Current scan is incomplete; event integrity is not yet verified.' });
    }
    const hash = new Sha256();
    let first = true, count = 0, last = 0, nextSequence = 1, terminal = false, observation = '', session = '';
    let missingSequences = 0, omittedEvents = 0;
    let bad = false;
    let deliveredCutoff: ObjectValue | undefined;
    try {
      for await (const line of readLines(eventFile, hash, signal, !!manifest)) {
        if (!line.trim()) { issue('invalid_line', 'Empty recording line skipped.'); bad = true; continue; }
        let value: unknown;
        try { value = JSON.parse(line); } catch { issue('invalid_line', 'Malformed JSON line skipped.'); bad = true; continue; }
        if (first && !manifest) {
          first = false;
          if (!object(value) || value.format !== INSPECTOR_HEADER.format || value.schema_version !== 1 || value.coverage !== INSPECTOR_HEADER.coverage || value.payloads !== 'not_included' || value.sensitive !== true) throw new Error('Missing or unsupported inspector JSONL header');
          continue;
        }
        first = false;
        if (!isXrayEnvelope(value)) { issue('unsupported_envelope', 'Unsupported, malformed, or unsafe-integer envelope skipped.'); bad = true; continue; }
        const currentSession = value.event.correlation.session_id;
        if (manifest && (value.observation_id !== manifest.observation_id || currentSession !== manifest.session_id)) { issue('foreign_envelope', 'Envelope identity differs from the bundle manifest.'); bad = true; continue; }
        if (count && (observation !== value.observation_id || session !== currentSession)) issue('mixed_observations', 'Export contains multiple observation or session identities.');
        if (terminal) { issue('after_terminal', 'Envelope follows the observed terminal cutoff.'); bad = true; }
        if (value.sequence !== nextSequence) {
          issue('sequence_gap', 'Sequence discontinuity, duplicate, or out-of-order evidence is present.');
          if (value.sequence < nextSequence) bad = true;
          else missingSequences += value.sequence - nextSequence;
        }
        nextSequence = value.sequence + 1;
        if (manifest) {
          for (const reference of declaredReferences(value)) {
            const stored = Object.hasOwn(manifest.payloads, reference.payload_id) ? manifest.payloads[reference.payload_id] : undefined;
            if (!stored || refKey(reference) !== refKey(stored.reference)) { issue('reference_mismatch', 'Declared event payload reference does not match the bundle manifest.'); bad = true; }
          }
        }
        if (value.event.domain === 'xray' && value.event.name === 'gap') {
          const data = value.event.data;
          if (object(data) && data.first_sequence === value.sequence && integer(data.last_sequence) && data.last_sequence >= value.sequence && data.last_sequence < Number.MAX_SAFE_INTEGER) {
            nextSequence = data.last_sequence + 1;
            missingSequences += data.last_sequence - value.sequence + 1;
          }
          else { issue('invalid_gap', 'Unsupported capture gap range.'); bad = true; }
        }
        if (value.event.domain === 'xray' && value.event.name === 'finalized') {
          try {
            validateFinalization(value.event.data, value.observation_id, value.sequence); terminal = true;
            deliveredCutoff = value.event.data as ObjectValue;
            if (deliveredCutoff.dropped_events || deliveredCutoff.hub_contention_drops || deliveredCutoff.evicted_payloads) issue('capture_loss', 'Cutoff receipt reports diagnostic drops, contention uncertainty, or payload eviction.');
          }
          catch { issue('invalid_cutoff', 'Unsupported observed cutoff receipt.'); bad = true; }
        }
        if (value.event.name.endsWith('_omitted')) omittedEvents++;
        if (value.event.name.includes('omitted') || value.event.name === 'gap') issue('capture_loss', 'Capture reports omitted or missing evidence.');
        observation = value.observation_id; session = currentSession; last = value.sequence; count++;
        yield value;
      }
      if (first && !manifest) throw new Error('Empty inspector export');
      if (manifest) {
        if (manifest.missing_sequences !== missingSequences || manifest.omitted_events !== omittedEvents) { issue('loss_metadata_mismatch', 'Declared loss counts differ from delivered evidence.'); bad = true; }
        const receipt = (manifest as unknown as ObjectValue).finalization;
        if (object(receipt) && (!deliveredCutoff || ['observation_id', 'final_sequence', 'dropped_events', 'hub_contention_drops', 'evicted_payloads'].some((key) => receipt[key] !== deliveredCutoff?.[key]))) { issue('cutoff_mismatch', 'Manifest cutoff receipt differs from the delivered event.'); bad = true; }
        if (manifest.state === 'finalized' && !terminal) { issue('missing_cutoff', 'Manifest claims finalization without a matching delivered cutoff.'); bad = true; }
        const pending = issues.findIndex((entry) => entry.kind === 'event_integrity_pending');
        if (pending >= 0) issues.splice(pending, 1);
        if (bad || eventFile.size !== manifest.events.byte_len || hash.hex() !== manifest.events.sha256 || count !== manifest.events.envelope_count || last !== manifest.events.last_sequence) issue('event_integrity_failed', 'Event bytes, digest, count, or sequence do not match the manifest.');
      }
    } catch (error) {
      if (signal?.aborted) throw error;
      issue('read_failed', 'Recording scan stopped: unsupported format, invalid UTF-8, overlong line, or unreadable file.');
    }
  }
  return {
    issues, events,
    async search(query, options = {}) {
      const { offset = 0, limit = 100, maxScanned = 100_000, signal } = options;
      if (query.length > 1024 || !integer(offset) || offset > 1_000_000 || !integer(limit) || limit < 1 || limit > 1000 || !integer(maxScanned) || maxScanned < 1 || maxScanned > 1_000_000) throw new Error('Search bound exceeded');
      const found: XrayEnvelope[] = [];
      let scanned = 0, matched = 0, resultBytes = 0, hasMore = false;
      const terms = parseQuery(query);
      for await (const event of events(signal)) {
        if (scanned >= maxScanned) { hasMore = true; break; }
        scanned++;
        if (matchesQuery(searchDocument(event), terms) && matched++ >= offset) {
          const bytes = encoder.encode(JSON.stringify(event)).length;
          if (found.length === limit || resultBytes + bytes > RECORDING_LIMITS.retainedBytes) { hasMore = true; break; }
          found.push(event); resultBytes += bytes;
        }
      }
      return { events: found, scanned, hasMore, issues: [...issues] };
    },
    async readPayload(id) {
      const entry = manifest && Object.hasOwn(manifest.payloads, id) ? manifest.payloads[id] : undefined;
      if (!entry || !entry.relative_path) throw new Error('Payload is not locally retained');
      const file = relative.get(entry.relative_path);
      if (!file || file.size !== entry.reference.byte_len || file.size > RECORDING_LIMITS.payloadBytes) throw new Error('Payload missing, size mismatch, or payload limit exceeded');
      let bytes: Uint8Array;
      try { bytes = new Uint8Array(await file.arrayBuffer()); }
      catch { issue('payload_read_failed', 'Explicitly opened payload file could not be read.'); throw new Error('Payload file read failed'); }
      const hash = new Sha256(); hash.update(bytes);
      if (hash.hex() !== entry.reference.sha256) { issue('payload_integrity_failed', 'Explicitly opened payload failed digest verification.'); throw new Error('Payload integrity mismatch'); }
      return bytes;
    },
  };
}

/** Bounded chunks and line assembly, including files with no newline or malicious UTF-8. */
async function* readLines(file: File, hash: Sha256, signal?: AbortSignal, requireFinalNewline = false): AsyncGenerator<string> {
  const line = new Uint8Array(RECORDING_LIMITS.lineBytes);
  let used = 0;
  const decoder = new TextDecoder('utf-8', { fatal: true });
  for (let offset = 0; offset < file.size; offset += RECORDING_LIMITS.chunkBytes) {
    signal?.throwIfAborted();
    const chunk = new Uint8Array(await file.slice(offset, offset + RECORDING_LIMITS.chunkBytes).arrayBuffer());
    hash.update(chunk);
    for (let start = 0, end = 0; end <= chunk.length; end++) {
      if (end !== chunk.length && chunk[end] !== 10) continue;
      const size = end - start;
      if (used + size > line.length) throw new Error('Line limit exceeded');
      line.set(chunk.subarray(start, end), used); used += size;
      if (end < chunk.length) { yield decoder.decode(line.subarray(0, used)); used = 0; }
      start = end + 1;
    }
  }
  if (used) {
    if (requireFinalNewline) throw new Error('Incomplete bundle JSONL tail');
    yield decoder.decode(line.subarray(0, used));
  }
}

/** Convenience UI open: retains a capped prefix. Use the reader for full-file bounded search. */
export async function openRecording(files: File[]): Promise<{ events: XrayEnvelope[]; issues: EvidenceIssue[] }> {
  const reader = await createRecordingReader(files);
  const events: XrayEnvelope[] = [];
  let bytes = 0, capped = false;
  for await (const event of reader.events()) {
    const size = encoder.encode(JSON.stringify(event)).length;
    if (events.length >= RECORDING_LIMITS.retainedEvents || bytes + size > RECORDING_LIMITS.retainedBytes) { capped = true; break; }
    events.push(event); bytes += size;
  }
  return { events, issues: [...reader.issues, ...(capped ? [{ kind: 'retained_window', message: 'Only a bounded prefix is loaded; remaining evidence was not scanned or integrity-verified. Use incremental recording search.' }] : [])] };
}
