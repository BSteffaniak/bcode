import { InspectorWorkerEngine, type WorkerCommand, type WorkerReply } from './core/store';
const engine = new InspectorWorkerEngine();
const scope = globalThis as unknown as { onmessage: (event: MessageEvent<WorkerCommand>) => void; postMessage: (reply: WorkerReply) => void };
scope.onmessage = event => scope.postMessage(engine.handle(event.data));
