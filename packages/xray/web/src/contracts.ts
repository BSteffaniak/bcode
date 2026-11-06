export interface XrayCorrelation {
  session_id: string;
  turn_id?: string | null;
  request_id?: string | null;
  tool_call_id?: string | null;
  run_id?: string | null;
  activation_id?: string | null;
  attempt_id?: string | null;
}

/** Raw X-ray v1 evidence, not canonical session state or an execution command. */
export interface XrayEnvelope {
  schema_version: 1;
  observation_id: string;
  sequence: number;
  timestamp_unix_ms: number;
  event: {
    domain: string;
    name: string;
    correlation: XrayCorrelation;
    data: unknown;
    sensitive: boolean;
    capture?: unknown;
  };
}

/** Exchanged only in memory by the same-origin local launcher's POST bootstrap. */
export interface InspectorBootstrap {
  schema_version: 1;
  endpoint: string;
  /** Gateway bearer for gateway endpoints; never a daemon credential in gateway mode. */
  bearer: string;
  session_id: string;
  deep_capture: boolean;
  /** Synthetic evidence must be visibly distinguished from real observations. */
  demo?: boolean;
}

export type SearchScope = 'retained-live' | 'opened-recording';
export interface EvidenceIssue {
  kind: string;
  message: string;
  observation_id?: string;
  sequence?: number;
}
