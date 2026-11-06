import type { XrayEnvelope } from '../contracts';
export interface Term { field: string; value: string; negate: boolean }
const fields = new Set(['domain', 'name', 'event', 'session', 'turn', 'request', 'tool', 'run', 'activation', 'attempt', 'observation', 'sequence', 'sensitive', 'data']);
export function parseQuery(query: string): Term[] {
  if (query.length > 4096) throw new Error('Query exceeds 4096 characters');
  const terms: Term[] = []; let i = 0;
  while (i < query.length) {
    if (/\s/.test(query[i])) { i++; continue; }
    let negate = false; if (query[i] === '-') { negate = true; i++; }
    let value = ''; let quoted = false; let separator = -1;
    while (i < query.length && (quoted || !/\s/.test(query[i]))) {
      const c = query[i++];
      if (c === '"') { quoted = !quoted; continue; }
      if (c === ':' && !quoted && separator < 0) separator = value.length;
      if (c === '\\') { if (i === query.length) throw new Error('Trailing escape'); value += query[i++]; }
      else value += c;
    }
    if (quoted) throw new Error('Unclosed quote');
    const colon = separator; const field = colon < 0 ? '' : value.slice(0, colon).toLowerCase();
    if (field && !fields.has(field)) throw new Error(`Unknown search field: ${field}`);
    value = (colon < 0 ? value : value.slice(colon + 1)).toLowerCase();
    if (!value) throw new Error('Empty search term');
    terms.push({ field, value, negate });
    if (terms.length > 64) throw new Error('Too many search terms');
  }
  return terms;
}
export function searchDocument(e: XrayEnvelope): Record<string, string> {
  const c = e.event.correlation;
  return Object.fromEntries(Object.entries({ domain: e.event.domain, name: e.event.name, event: `${e.event.domain}.${e.event.name}`, session: c.session_id, turn: c.turn_id ?? '', request: c.request_id ?? '', tool: c.tool_call_id ?? '', run: c.run_id ?? '', activation: c.activation_id ?? '', attempt: c.attempt_id ?? '', observation: e.observation_id, sequence: String(e.sequence), sensitive: String(e.event.sensitive), data: JSON.stringify(e.event.data), '': JSON.stringify(e) }).map(([k, v]) => [k, v.toLowerCase()]));
}
export const matchesQuery = (doc: Record<string, string>, terms: Term[]): boolean => terms.every(t => doc[t.field].includes(t.value) !== t.negate);
