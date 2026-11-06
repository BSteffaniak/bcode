import type { XrayEnvelope } from '../contracts';
import { encodeEvent, INSPECTOR_HEADER, RECORDING_LIMITS } from './index';

/** Structural subset avoids requiring experimental DOM type libraries. */
export interface WritableRecordingFile {
  write(data: string): Promise<void>;
  close(): Promise<void>;
  abort(reason?: unknown): Promise<void>;
}
export interface RecordingStatus {
  state: 'recording' | 'closing' | 'closed' | 'failed' | 'aborted';
  queuedBytes: number;
  writtenBytes: number;
  writtenEvents: number;
  /** Fixed public-safe diagnostic, never an OS path or raw exception. */
  failure?: string;
}
export interface LiveRecordingSink {
  /** Never waits for disk. False means capture failed/stopped; no implicit retry. */
  append(event: XrayEnvelope): boolean;
  close(): Promise<RecordingStatus>;
  abort(): Promise<RecordingStatus>;
  status(): RecordingStatus;
}
interface PickerWindow {
  isSecureContext?: boolean;
  navigator?: { userActivation?: { isActive: boolean } };
  showSaveFilePicker?: (options: unknown) => Promise<{ createWritable(): Promise<WritableRecordingFile> }>;
}
const pickerWindow = (): PickerWindow => globalThis as unknown as PickerWindow;

export function supportsLiveRecording(): boolean {
  const host = pickerWindow();
  return host.isSecureContext === true && typeof host.showSaveFilePicker === 'function';
}

/** Invoke directly in an explicit click handler; never called on connect or reconnect. */
export async function pickLiveRecording(onStatus?: (status: RecordingStatus) => void): Promise<LiveRecordingSink> {
  const host = pickerWindow();
  if (!supportsLiveRecording() || host.navigator?.userActivation?.isActive !== true) throw new Error('Disk recording requires a supported browser and an explicit user gesture; use bounded export instead');
  let writer: WritableRecordingFile;
  try {
    const handle = await host.showSaveFilePicker!({ suggestedName: 'xray-incomplete-window.jsonl', types: [{ description: 'Sensitive X-ray diagnostic window', accept: { 'application/x-ndjson': ['.jsonl'] } }] });
    writer = await handle.createWritable();
  } catch {
    throw new Error('Recording file permission was declined or the file could not be opened');
  }
  if (!writer || typeof writer.write !== 'function' || typeof writer.close !== 'function' || typeof writer.abort !== 'function') throw new Error('Writable recording streams are unavailable; use bounded export instead');
  return createLiveRecording(writer, onStatus);
}

/** Explicitly supplied/permissioned disk stream. No memory fallback or sensitive auto-fetch. */
export async function createLiveRecording(writer: WritableRecordingFile, onStatus?: (status: RecordingStatus) => void): Promise<LiveRecordingSink> {
  const maxQueuedBytes = 4 * 1024 ** 2;
  const maxQueuedEvents = 128;
  const encoder = new TextEncoder();
  let current: RecordingStatus = { state: 'recording', queuedBytes: 0, writtenBytes: 0, writtenEvents: 0 };
  const queue: { line: string; bytes: number }[] = [];
  let pump: Promise<void> | undefined;
  let finish: Promise<RecordingStatus> | undefined;
  let aborting: Promise<void> | undefined;
  let acceptedBytes = 0;
  const status = () => ({ ...current });
  const notify = () => { try { onStatus?.(status()); } catch { /* UI callbacks never control persistence semantics. */ } };
  const abortWriter = () => aborting ??= (async () => { try { await writer.abort(); } catch { /* Failure remains explicit in status. */ } })();
  const fail = (failure: string) => {
    current = { ...current, state: 'failed', queuedBytes: 0, failure };
    queue.length = 0;
    notify();
    void abortWriter();
  };
  const drain = async () => {
    while (queue.length && (current.state === 'recording' || current.state === 'closing')) {
      const next = queue[0];
      try { await writer.write(next.line); } catch { fail('Disk write failed; local recording is incomplete.'); break; }
      if (current.state !== 'recording' && current.state !== 'closing') break;
      queue.shift();
      current = { ...current, queuedBytes: current.queuedBytes - next.bytes, writtenBytes: current.writtenBytes + next.bytes, writtenEvents: current.writtenEvents + 1 };
      notify();
    }
    if (current.state === 'failed' || current.state === 'aborted') await abortWriter();
  };
  const startPump = () => {
    if (!pump) pump = drain().finally(() => {
      pump = undefined;
      if (queue.length && (current.state === 'recording' || current.state === 'closing')) startPump();
    });
  };
  try {
    const header = JSON.stringify(INSPECTOR_HEADER) + '\n';
    await writer.write(header);
    current.writtenBytes = encoder.encode(header).length;
    acceptedBytes = current.writtenBytes;
  } catch {
    fail('Recording header write failed; no recording was started.');
    await abortWriter();
    throw new Error('Recording header write failed');
  }
  notify();
  return {
    status,
    append(event) {
      if (current.state !== 'recording') return false;
      let line: string;
      try { line = encodeEvent(event); } catch { fail('Unsupported or oversized envelope; recording stopped incomplete.'); if (!pump) void abortWriter(); return false; }
      const bytes = encoder.encode(line).length;
      if (queue.length >= maxQueuedEvents || current.queuedBytes + bytes > maxQueuedBytes || acceptedBytes + bytes > RECORDING_LIMITS.totalBytes) {
        fail('Disk recording capacity exceeded; recording stopped incomplete without blocking observation.');
        if (!pump) void abortWriter();
        return false;
      }
      acceptedBytes += bytes;
      queue.push({ line, bytes });
      current = { ...current, queuedBytes: current.queuedBytes + bytes };
      startPump(); notify();
      return true;
    },
    close() {
      if (finish) return finish;
      finish = (async () => {
        if (current.state === 'recording') { current = { ...current, state: 'closing' }; notify(); }
        while (pump) await pump;
        if (current.state === 'closing') {
          try {
            await writer.close();
            if (current.state === 'closing') current = { ...current, state: 'closed' };
          }
          catch { fail('Disk close failed; local flush is not certified.'); await abortWriter(); }
          notify();
        } else if (current.state === 'failed' || current.state === 'aborted') await abortWriter();
        return status();
      })();
      return finish;
    },
    async abort() {
      if (current.state === 'closed') return status();
      if (current.state !== 'failed') current = { ...current, state: 'aborted', queuedBytes: 0 };
      queue.length = 0; notify();
      await abortWriter();
      return status();
    },
  };
}
