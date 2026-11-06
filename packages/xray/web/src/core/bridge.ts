import type { InspectorSnapshot } from '../api';
import type { WorkerCommand, WorkerReply } from './store';
type CommandBody = WorkerCommand extends infer C ? C extends WorkerCommand ? Omit<C, 'epoch' | 'id'> : never : never;
export interface WorkerPort {
  postMessage(message: WorkerCommand): void;
  onmessage: ((event: MessageEvent<WorkerReply>) => void) | null;
  onerror: ((event: ErrorEvent) => void) | null;
  terminate(): void;
}
/** Bounded request/ack bridge. Ingestion awaits acknowledgments instead of flooding postMessage. */
export class StoreBridge {
  private next = 0;
  private pending = new Map<number, { resolve: () => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }>();
  epoch = 0;
  constructor(private readonly port: WorkerPort, private readonly onSnapshot: (snapshot: InspectorSnapshot) => void) {
    port.onmessage = ({ data }) => {
      const pending = this.pending.get(data.id); if (!pending) return;
      this.pending.delete(data.id); clearTimeout(pending.timer);
      if (data.error) { pending.reject(new Error(data.error)); return; }
      if (data.epoch === this.epoch && data.snapshot) this.onSnapshot(data.snapshot);
      pending.resolve();
    };
    port.onerror = () => this.fail('Inspector worker failed');
  }
  send(body: CommandBody, epoch = this.epoch): Promise<void> {
    if (this.pending.size >= 8) return Promise.reject(new Error('Worker control queue is full'));
    const id = ++this.next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error('Inspector worker timed out')); }, 15_000);
      this.pending.set(id, { resolve, reject, timer });
      try { this.port.postMessage({ ...body, epoch, id } as WorkerCommand); }
      catch { clearTimeout(timer); this.pending.delete(id); reject(new Error('Inspector worker unavailable')); }
    });
  }
  reset(scope: InspectorSnapshot['scope']): Promise<void> { this.epoch++; return this.send({ kind: 'reset', scope }); }
  dispose(): void { this.fail('Inspector disposed'); this.port.terminate(); }
  private fail(message: string): void { for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error(message)); } this.pending.clear(); }
}
