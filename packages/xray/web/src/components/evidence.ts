import type { XrayEnvelope } from '../contracts';

export const eventId = (event: XrayEnvelope): string => `${event.observation_id}:${event.sequence}`;
export const record = (value: unknown): Record<string, unknown> | null => value !== null && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : null;
export const pretty = (value: unknown): string => JSON.stringify(value, null, 2) ?? 'null';
export const queryValue = (value: string): string => JSON.stringify(value);

export interface PayloadReference {
  schema_version: 1; payload_id: string; session_id: string; content_type: string; byte_len: number; sha256: string;
}
/** Resolve only exact, versioned declarations. Never scan content for reference-shaped keys. */
export function declaredPayloads(envelope: XrayEnvelope): { references: PayloadReference[]; status: string } {
  const capture = record(envelope.event.capture);
  if (!capture) return { references: [], status: 'Legacy opaque content — no payload declarations.' };
  const invalid = { references: [], status: 'Unsupported or invalid payload declarations; fetching disabled.' };
  if (capture.schema_version !== 1 || Object.keys(capture).some(key => !['schema_version', 'attachments'].includes(key)) || !Array.isArray(capture.attachments)) return invalid;
  const pointers = new Set<string>();
  const references = new Map<string, PayloadReference>();
  for (const item of capture.attachments) {
    const attachment = record(item);
    if (!attachment || attachment.kind !== 'payload_ref' || typeof attachment.pointer !== 'string' || Object.keys(attachment).some(key => !['kind', 'pointer'].includes(key))) return invalid;
    const pointer = attachment.pointer;
    if ((pointer !== '' && !pointer.startsWith('/')) || /~(?:[^01]|$)/u.test(pointer) || pointers.has(pointer)) return invalid;
    pointers.add(pointer);
    let target: unknown = envelope.event.data;
    for (const encoded of pointer === '' ? [] : pointer.slice(1).split('/')) {
      const key = encoded.replace(/~1/gu, '/').replace(/~0/gu, '~');
      if (Array.isArray(target)) {
        if (!/^(0|[1-9]\d*)$/u.test(key)) return invalid;
        target = target[Number(key)];
      } else {
        const object = record(target);
        if (!object || !Object.hasOwn(object, key)) return invalid;
        target = object[key];
      }
    }
    const ref = record(target);
    if (!ref || ref.schema_version !== 1 || typeof ref.payload_id !== 'string' || !ref.payload_id || ref.session_id !== envelope.event.correlation.session_id || typeof ref.content_type !== 'string' || !ref.content_type || typeof ref.byte_len !== 'number' || !Number.isSafeInteger(ref.byte_len) || ref.byte_len < 0 || typeof ref.sha256 !== 'string' || !/^[a-f0-9]{64}$/u.test(ref.sha256) || Object.keys(ref).some(key => !['schema_version', 'payload_id', 'session_id', 'content_type', 'byte_len', 'sha256'].includes(key))) return invalid;
    const reference = ref as unknown as PayloadReference;
    if (references.has(reference.payload_id) && pretty(references.get(reference.payload_id)) !== pretty(reference)) return invalid;
    references.set(reference.payload_id, reference);
  }
  return { references: [...references.values()], status: references.size ? 'Declared sensitive payloads. Fetch explicitly; availability is temporary.' : 'Explicitly declared inline/control content; no retained payloads.' };
}

export function streamEvidence(events: XrayEnvelope[], selected: XrayEnvelope): XrayEnvelope[] {
  const correlation = selected.event.correlation;
  if (!correlation.request_id) return [];
  return events.filter(item => item.observation_id === selected.observation_id && item.event.correlation.session_id === correlation.session_id && item.event.correlation.request_id === correlation.request_id && item.event.correlation.attempt_id === correlation.attempt_id);
}
export function streamText(event: XrayEnvelope): string {
  const data = record(event.event.data);
  for (const key of ['text', 'delta', 'content', 'chunk']) if (typeof data?.[key] === 'string') return data[key] as string;
  return pretty(event.event.data);
}
