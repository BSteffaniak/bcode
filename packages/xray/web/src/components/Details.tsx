import { useEffect, useState } from 'react';
import type { XrayEnvelope } from '../contracts';
import { declaredPayloads, eventId, pretty, streamEvidence, streamText } from './evidence';

interface Props {
  event: XrayEnvelope | null;
  events: XrayEnvelope[];
  readPayload: (ref: unknown) => Promise<Uint8Array>;
  onFilter: (key: string, value: string) => void;
  evicted: boolean;
}
export function Details({ event, events, readPayload, onFilter, evicted }: Props) {
  const [tab, setTab] = useState('event');
  const [payload, setPayload] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState(false);
  useEffect(() => { setPayload(null); setLoading(false); setFailure(false); }, [event]);
  if (!event) return <aside className="details empty"><span className="empty-symbol">⌘</span><h2>Look closer</h2><p>Select an event to inspect its evidence, correlations, and declared payloads.</p><p>Observation only. No execution controls.</p></aside>;
  const capture = declaredPayloads(event);
  const stream = streamEvidence(events, event);
  return <aside className="details" aria-label="Event details">
    <div className="detail-heading"><small>EVENT DETAILS</small><h2>{event.event.name}</h2><span className="mono">{event.observation_id} · #{event.sequence}</span></div>
    {evicted && <div className="notice">Selection is outside the current window or evicted. This is a retained UI copy, not current search coverage.</div>}
    <nav className="tabs" aria-label="Detail view">{['event', 'stream', 'payload'].map(name => <button key={name} aria-pressed={tab === name} onClick={() => setTab(name)}>{name}</button>)}</nav>
    <div className="detail-body">
      {tab === 'event' && <><h3>Correlations <small>filter by supplied ID</small></h3><small>Filters use case-insensitive substring matching, not identity assertions.</small><dl className="correlations">{Object.entries(event.event.correlation).filter((entry): entry is [string, string] => typeof entry[1] === 'string').map(([key, value]) => <div key={key}><dt>{key}</dt><dd><button className="link mono" onClick={() => onFilter(key, value)} title={`Filter ${key}: ${value}`}>{value}</button></dd></div>)}</dl><h3>Observed envelope</h3><pre tabIndex={0}>{pretty(event)}</pre></>}
      {tab === 'stream' && <><h3>Request / attempt evidence</h3><div className="notice">Current window only; not reconstructed history. Completion and sequence continuity are not inferred from event names. Missing completion or gaps remain unresolved.</div>{!event.event.correlation.request_id ? <p>This event has no request ID. No stream association is inferred.</p> : <><p>{stream.length} matching events · attempt {event.event.correlation.attempt_id ?? 'not supplied'}</p>{stream.map(item => <section className="stream-part" key={eventId(item)}><small>#{item.sequence} · {item.event.domain} / {item.event.name}</small><pre>{streamText(item)}</pre></section>)}</>}</>}
      {tab === 'payload' && <><div className="notice">{capture.status}</div>{capture.references.map(ref => <section className="payload-card" key={ref.payload_id}><h3>{ref.content_type}</h3><p className="mono">{ref.payload_id} · {ref.byte_len.toLocaleString()} bytes</p><small className="mono digest">SHA-256 {ref.sha256}</small><button disabled={loading} onClick={() => { setLoading(true); setFailure(false); setPayload(null); void readPayload(ref).then(bytes => setPayload(new TextDecoder().decode(bytes))).catch(() => setFailure(true)).finally(() => setLoading(false)); }}>{loading ? 'Reading…' : 'Read sensitive payload'}</button></section>)}{failure && <div role="alert" className="notice error">Payload unavailable or integrity validation failed. Missing bytes are not empty content.</div>}{payload !== null && <><h3>Plain text view <small>never executed as HTML</small></h3><pre tabIndex={0}>{payload}</pre></>}</>}
    </div>
  </aside>;
}
