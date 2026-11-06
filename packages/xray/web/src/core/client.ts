import type { InspectorController, InspectorSnapshot } from '../api';
import type { EvidenceIssue, InspectorBootstrap, XrayEnvelope } from '../contracts';
import { StoreBridge, type WorkerPort } from './bridge';
import { ObservationTransport, validateBootstrap } from './transport';
export interface InspectorClientOptions { worker?: WorkerPort; fetch?: typeof fetch }
/** One-way, epoch-isolated inspector. Credentials live only in the active transport. */
export class InspectorClient implements InspectorController {
  private readonly bridge: StoreBridge;
  private transport?: ObservationTransport;
  private disposed = false;
  private operation = 0;
  private query?: string;
  private window?: [number, number];
  private controlRunning = false;
  constructor(private readonly onSnapshot: (s: InspectorSnapshot) => void, private readonly options: InspectorClientOptions = {}) {
    const worker = options.worker ?? new Worker(new URL('../worker.ts', import.meta.url), { type: 'module' }) as unknown as WorkerPort;
    this.bridge = new StoreBridge(worker, onSnapshot);
    this.command({ kind: 'state', state: 'idle' });
  }
  async connect(bootstrap: InspectorBootstrap): Promise<void> {
    if (this.disposed) throw new Error('Inspector disposed');
    validateBootstrap(bootstrap);
    const operation = ++this.operation;
    this.transport?.dispose();
    await this.bridge.reset('retained-live');
    if (operation !== this.operation) return;
    const epoch = this.bridge.epoch;
    await this.bridge.send({ kind: 'state', state: 'connecting' }, epoch);
    const transport = new ObservationTransport({ ...bootstrap }, {
      events: async events => { if (operation === this.operation) await this.bridge.send({ kind: 'ingest', events }, epoch); },
      state: state => { if (operation === this.operation) this.command({ kind: 'state', state }, epoch); },
      issue: message => { if (operation === this.operation) this.command({ kind: 'issue', issue: { kind: 'transport', message } }, epoch); },
    }, this.options.fetch);
    this.transport = transport;
    await transport.connect();
  }
  async stop(): Promise<void> {
    try { await this.transport?.stop(); }
    catch { this.command({ kind: 'issue', issue: { kind: 'cutoff-pending', message: 'Observation cutoff/release did not settle; retry Stop. This says nothing about execution completion.' } }); throw new Error('Observation cutoff or release pending'); }
  }
  dispose(): void { this.disposed = true; this.operation++; this.transport?.dispose(); this.transport = undefined; this.bridge.dispose(); }
  setQuery(query: string): void { if (this.disposed) return; this.query = query.length > 4096 ? query.slice(0, 4097) : query; this.flushControls(); }
  setWindow(offset: number, limit: number): void { if (this.disposed) return; this.window = [offset, limit]; this.flushControls(); }
  private flushControls(): void {
    if (this.controlRunning) return;
    this.controlRunning = true;
    const run = async () => {
      try {
        while (!this.disposed && (this.query !== undefined || this.window !== undefined)) {
          const query = this.query; const window = this.window; this.query = undefined; this.window = undefined;
          if (query !== undefined) await this.bridge.send({ kind: 'query', query });
          if (window) await this.bridge.send({ kind: 'window', offset: window[0], limit: window[1] });
        }
      } catch { /* Surface failure through a bounded local snapshot, never raw tool data. */ this.failure(); }
      finally { this.controlRunning = false; }
    };
    void run();
  }
  loadRecording(events: XrayEnvelope[], issues: EvidenceIssue[] = []): void {
    if (this.disposed) return;
    const operation = ++this.operation; this.transport?.dispose(); this.transport = undefined;
    const run = async () => {
      await this.bridge.reset('opened-recording'); const epoch = this.bridge.epoch;
      await this.bridge.send({ kind: 'state', state: 'recording' }, epoch);
      // One bounded message in flight; large files never become a worker-control queue.
      for (let offset = 0; offset < events.length; offset += 1) {
        if (operation !== this.operation) return;
        await this.bridge.send({ kind: 'ingest', events: events.slice(offset, offset + 1) }, epoch);
      }
      for (const issue of issues.slice(0, 256)) {
        if (operation !== this.operation) return;
        await this.bridge.send({ kind: 'issue', issue: { kind: String(issue.kind).slice(0, 128), message: String(issue.message).slice(0, 4096), observation_id: issue.observation_id?.slice(0, 4096), sequence: issue.sequence } }, epoch);
      }
    };
    void run().catch(() => { if (operation === this.operation) this.command({ kind: 'issue', issue: { kind: 'invalid-recording', message: 'Recording is invalid, incompatible, or discontinuous; only the accepted bounded prefix is shown' } }); });
  }
  readPayload(ref: unknown): Promise<Uint8Array> { return this.transport?.readPayload(ref) ?? Promise.reject(new Error('Live payload lease unavailable')); }
  canReadPayload(): boolean { return this.transport?.canReadPayload() ?? false; }
  private command(command: Parameters<StoreBridge['send']>[0], epoch = this.bridge.epoch): void { void this.bridge.send(command, epoch).catch(() => this.failure()); }
  private failure(): void {
    if (!this.disposed) this.onSnapshot({ state: 'disconnected', scope: 'retained-live', events: [], issues: [{ kind: 'worker', message: 'Inspector store unavailable or control queue exceeded its bound' }], retained: 0, retainedBytes: 0, received: 0, evicted: 0, matches: 0, queryError: null });
  }
}
