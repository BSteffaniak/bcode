import type { InspectorSnapshot } from '../api';
import type { EvidenceIssue, XrayEnvelope } from '../contracts';
import { envelope, MAX_FRAME_BYTES, SequenceTracker } from './protocol';
import { matchesQuery, parseQuery, searchDocument, type Term } from './search';
interface Row { event: XrayEnvelope; bytes: number; doc: Record<string, string>; match: boolean }
export class EvidenceStore {
  private rows = new Map<number, Row>();
  private nextId = 0;
  private bytes = 0;
  private received = 0;
  private evicted = 0;
  private matches = 0;
  private terms: Term[] = [];
  private queryError: string | null = null;
  private offset = 0;
  private limit = 200;
  private issues: EvidenceIssue[] = [];
  private trackers = new Map<string, SequenceTracker>();
  state: InspectorSnapshot['state'] = 'idle';
  scope: InspectorSnapshot['scope'] = 'retained-live';
  constructor(private readonly maxCount = 20_000, private readonly maxBytes = 32 * 1024 * 1024) {}
  reset(scope: InspectorSnapshot['scope']): void {
    this.rows.clear(); this.trackers.clear(); this.bytes = 0; this.received = 0; this.evicted = 0; this.matches = 0; this.issues = []; this.offset = 0; this.scope = scope; this.state = scope === 'opened-recording' ? 'recording' : 'idle';
  }
  issue(issue: EvidenceIssue): void { if (this.issues.length === 256) this.issues.shift(); this.issues.push(issue); }
  ingest(raw: unknown): void {
    const e = envelope(raw); const encoded = JSON.stringify(e); const size = new TextEncoder().encode(encoded).length;
    if (size > MAX_FRAME_BYTES) throw new Error('Event exceeds byte bound');
    let tracker = this.trackers.get(e.observation_id);
    if (!tracker) {
      if (this.trackers.size >= 256) throw new Error('Recording observation count exceeds bound');
      const initialSequence = this.scope === 'opened-recording' ? e.sequence : 1;
      tracker = new SequenceTracker(initialSequence); this.trackers.set(e.observation_id, tracker);
      if (initialSequence > 1) this.issue({ kind: 'recording-prefix-missing', message: `Recording begins at sequence ${initialSequence}; preceding evidence is unavailable`, observation_id: e.observation_id });
    }
    tracker.accept(e);
    this.received++;
    const doc = searchDocument(e);
    // Account for retained indexes as well as wire evidence (UTF-16 worst case).
    const bytes = size + Object.values(doc).reduce((n, value) => n + value.length * 2, 0);
    const match = matchesQuery(doc, this.terms);
    this.rows.set(this.nextId++, { event: e, bytes, doc, match }); this.bytes += bytes; if (match) this.matches++;
    while (this.rows.size > this.maxCount || this.bytes > this.maxBytes) {
      const oldest = this.rows.entries().next().value as [number, Row];
      this.rows.delete(oldest[0]); this.bytes -= oldest[1].bytes; if (oldest[1].match) this.matches--; this.evicted++;
    }
    if (e.event.domain === 'xray' && e.event.name === 'gap') this.issue({ kind: 'server-gap', message: 'Server reported missing diagnostic delivery', observation_id: e.observation_id, sequence: e.sequence });
    if (e.event.domain === 'xray' && e.event.name === 'finalized') {
      const d = e.event.data as Record<string, number>;
      if (d.hub_contention_drops) this.issue({ kind: 'capture-uncertainty', message: 'Cutoff reports presequence capture uncertainty', observation_id: e.observation_id });
      if (d.evicted_payloads) this.issue({ kind: 'payload-eviction', message: 'Server evicted retained payloads', observation_id: e.observation_id });
    }
  }
  query(query: string): void {
    try { this.terms = parseQuery(query); this.queryError = null; }
    catch (error) { this.queryError = (error as Error).message; return; }
    this.offset = 0; this.matches = 0;
    for (const row of this.rows.values()) { row.match = matchesQuery(row.doc, this.terms); if (row.match) this.matches++; }
  }
  window(offset: number, limit: number): void { this.offset = Number.isFinite(offset) ? Math.max(0, Math.floor(offset)) : 0; this.limit = Number.isFinite(limit) ? Math.min(2000, Math.max(0, Math.floor(limit))) : 200; }
  snapshot(): InspectorSnapshot {
    const events: XrayEnvelope[] = []; let skipped = 0;
    for (const row of this.rows.values()) if (row.match) { if (skipped++ < this.offset) continue; if (events.length >= this.limit) break; events.push(row.event); }
    const issues = [...this.issues];
    if (this.evicted) issues.push({ kind: 'browser-eviction', message: `${this.evicted} events evicted from browser retention; search covers retained evidence only` });
    return { state: this.state, scope: this.scope, events, issues, retained: this.rows.size, retainedBytes: this.bytes, received: this.received, evicted: this.evicted, matches: this.matches, queryError: this.queryError };
  }
}
export type WorkerCommand = { epoch: number; id: number } & (
  { kind: 'reset'; scope: InspectorSnapshot['scope'] } | { kind: 'ingest'; events: XrayEnvelope[] } | { kind: 'query'; query: string } | { kind: 'window'; offset: number; limit: number } | { kind: 'state'; state: InspectorSnapshot['state'] } | { kind: 'issue'; issue: EvidenceIssue }
);
export interface WorkerReply { epoch: number; id: number; snapshot?: InspectorSnapshot; error?: string }
/** Synchronous worker core permits deterministic tests without a browser Worker. */
export class InspectorWorkerEngine {
  private epoch = 0;
  constructor(readonly store = new EvidenceStore()) {}
  handle(command: WorkerCommand): WorkerReply {
    if (command.kind === 'reset' && command.epoch >= this.epoch) { this.epoch = command.epoch; this.store.reset(command.scope); }
    if (command.epoch !== this.epoch) return { epoch: command.epoch, id: command.id };
    try {
      switch (command.kind) {
        case 'ingest': if (command.events.length > 128) throw new Error('Worker batch exceeds bound'); for (const event of command.events) this.store.ingest(event); break;
        case 'query': this.store.query(command.query); break;
        case 'window': this.store.window(command.offset, command.limit); break;
        case 'state': this.store.state = command.state; break;
        case 'issue': this.store.issue(command.issue); break;
      }
      return { epoch: this.epoch, id: command.id, snapshot: this.store.snapshot() };
    } catch { return { epoch: this.epoch, id: command.id, error: 'Invalid or discontinuous evidence; ingestion stopped' }; }
  }
}
