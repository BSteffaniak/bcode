import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { XrayEnvelope } from '../contracts';
import { declaredPayloads, eventId, streamEvidence } from './evidence';
import { Details } from './Details';

const reference = { schema_version: 1, payload_id: 'payload', session_id: 'session', content_type: 'text/plain', byte_len: 4, sha256: 'a'.repeat(64) };
function envelope(data: unknown = {}): XrayEnvelope {
  return { schema_version: 1, observation_id: 'observation', sequence: 1, timestamp_unix_ms: 1, event: { domain: 'provider', name: 'chunk', sensitive: false, correlation: { session_id: 'session', request_id: 'request', attempt_id: 'one' }, data } };
}
describe('declared sensitive payload access', () => {
  it('never discovers reference-shaped arbitrary content', () => {
    expect(declaredPayloads(envelope({ reference })).references).toEqual([]);
  });
  it('resolves escaped RFC6901 declarations without recursive discovery', () => {
    const event = envelope({ 'a/b': { '~key': reference }, other: reference });
    event.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/a~1b/~0key' }] };
    expect(declaredPayloads(event).references).toEqual([reference]);
  });
  it.each([
    { schema_version: 2, attachments: [] },
    { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/bad~2' }] },
    { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/reference' }, { kind: 'payload_ref', pointer: '/reference' }] },
    { schema_version: 1, attachments: [{ kind: 'unknown', pointer: '/reference' }] },
  ])('fails closed for unsupported/malformed metadata', capture => {
    const event = envelope({ reference }); event.event.capture = capture;
    expect(declaredPayloads(event).references).toEqual([]);
  });
  it('rejects foreign sessions and inherited locations', () => {
    const event = envelope({ reference: { ...reference, session_id: 'foreign' } });
    event.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/reference' }] };
    expect(declaredPayloads(event).references).toEqual([]);
    event.event.data = {}; event.event.capture = { schema_version: 1, attachments: [{ kind: 'payload_ref', pointer: '/constructor' }] };
    expect(declaredPayloads(event).references).toEqual([]);
  });
});
describe('evidence presentation', () => {
  it('isolates request streams by observation, session and attempt', () => {
    const selected = envelope();
    const other = envelope(); other.event.correlation.attempt_id = 'two';
    const foreign = envelope(); foreign.observation_id = 'other';
    expect(streamEvidence([selected, other, foreign], selected)).toEqual([selected]);
    expect(eventId(foreign)).not.toEqual(eventId(selected));
  });
  it('escapes hostile text and reports out-of-window selection', () => {
    const event = envelope({ text: '<script>alert(1)</script>' });
    const html = renderToStaticMarkup(<Details event={event} events={[event]} readPayload={async () => new Uint8Array()} onFilter={() => undefined} evicted />);
    expect(html).not.toContain('<script>');
    expect(html).toContain('&lt;script&gt;');
    expect(html).toContain('outside the current window or evicted');
  });
  it('renders a usable idle details pane', () => {
    const html = renderToStaticMarkup(<Details event={null} events={[]} readPayload={async () => new Uint8Array()} onFilter={() => undefined} evicted={false} />);
    expect(html).toContain('No execution controls');
  });
});
