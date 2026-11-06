import type { EvidenceIssue, InspectorBootstrap, SearchScope, XrayEnvelope } from './contracts';

/** Stable browser-side integration contract shared by transport/store and React. */
export interface InspectorSnapshot {
  state: 'idle' | 'bootstrapping' | 'connecting' | 'live' | 'finalizing' | 'stopped' | 'disconnected' | 'recording';
  events: XrayEnvelope[];
  issues: EvidenceIssue[];
  retained: number;
  retainedBytes: number;
  received: number;
  evicted: number;
  matches: number;
  queryError: string | null;
  scope: SearchScope;
}

export interface InspectorController {
  /** Enter the memory-only ticket redemption phase before attaching transport. */
  beginConnection(): void;
  /** Surface a failed redemption without retrying or attaching. */
  failConnection(): void;
  connect(bootstrap: InspectorBootstrap): Promise<void>;
  stop(): Promise<void>;
  dispose(): void;
  setQuery(query: string): void;
  setWindow(offset: number, limit: number): void;
  loadRecording(events: XrayEnvelope[], issues?: EvidenceIssue[]): void;
}
