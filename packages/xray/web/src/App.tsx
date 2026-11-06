import { useEffect, useMemo, useRef, useState, type ChangeEvent } from 'react';
import type { InspectorSnapshot } from './api';
import { redeemPairing } from './components/pairing';
import { InspectorClient } from './core/client';
import { openRecording, exportEvents, createRecordingReader, type RecordingReader } from './recording/index';
import type { XrayEnvelope } from './contracts';
import { Details } from './components/Details';
import { eventId, queryValue } from './components/evidence';

const WINDOW = 100;
const empty: InspectorSnapshot = { state: 'idle', events: [], issues: [], retained: 0, retainedBytes: 0, received: 0, evicted: 0, matches: 0, queryError: null, scope: 'retained-live' };
interface Preferences { query: string; compact: boolean; saved: string[] }
function preferences(): Preferences {
  try {
    const value: unknown = JSON.parse(localStorage.getItem('xray-ui-v1') ?? 'null');
    if (value && typeof value === 'object') {
      const item = value as Partial<Preferences>;
      return { query: typeof item.query === 'string' ? item.query.slice(0, 2048) : '', compact: item.compact === true, saved: Array.isArray(item.saved) ? item.saved.filter((value): value is string => typeof value === 'string').slice(0, 12) : [] };
    }
  } catch { /* Storage is optional. */ }
  return { query: '', compact: false, saved: [] };
}
export default function App() {
  const [prefs, setPrefs] = useState(preferences);
  const [snapshot, setSnapshot] = useState(empty);
  const [follow, setFollow] = useState(true);
  const [paused, setPaused] = useState(false);
  const [frozen, setFrozen] = useState<InspectorSnapshot | null>(null);
  const [selected, setSelected] = useState<XrayEnvelope | null>(null);
  const [offset, setOffset] = useState(0);
  const [navigation, setNavigation] = useState('activities');
  const [notice, setNotice] = useState('');
  const [working, setWorking] = useState(false);
  const [pairingCode, setPairingCode] = useState('');
  const client = useRef<InspectorClient | null>(null);
  const search = useRef<HTMLInputElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const directoryInput = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const recordingReader = useRef<RecordingReader | null>(null);
  const [offlineText, setOfflineText] = useState('');
  const [offlineOffset, setOfflineOffset] = useState(0);
  const [offlineMore, setOfflineMore] = useState(false);
  const display = paused && frozen ? frozen : snapshot;
  const lastPage = Math.max(0, snapshot.matches - WINDOW);
  useEffect(() => {
    const instance = new InspectorClient(setSnapshot);
    client.current = instance;
    return () => { instance.dispose(); client.current = null; };
  }, []);
  useEffect(() => { client.current?.setQuery(prefs.query); setOffset(0); }, [prefs.query]);
  useEffect(() => { if (follow && !paused) setOffset(lastPage); }, [lastPage, follow, paused]);
  useEffect(() => { client.current?.setWindow(offset, WINDOW); if (list.current) list.current.scrollTop = 0; }, [offset]);
  useEffect(() => { try { localStorage.setItem('xray-ui-v1', JSON.stringify(prefs)); } catch { /* Private browsing may deny storage. */ } }, [prefs]);
  useEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key === 'k') { event.preventDefault(); search.current?.focus(); }
    };
    window.addEventListener('keydown', shortcut);
    return () => window.removeEventListener('keydown', shortcut);
  }, []);
  const groups = useMemo(() => {
    const result = new Map<string, { domain: string; count: number; first: number; last: number; key: string; value: string }>();
    for (const item of display.events) {
      const correlation = item.event.correlation;
      const key = correlation.run_id ? 'run_id' : 'session_id';
      const value = correlation.run_id ?? correlation.session_id;
      const id = `${key}:${value}`;
      const previous = result.get(id);
      result.set(id, { domain: key === 'run_id' ? 'Workflow' : 'Session', count: (previous?.count ?? 0) + 1, first: Math.min(previous?.first ?? Infinity, item.timestamp_unix_ms), last: Math.max(previous?.last ?? -Infinity, item.timestamp_unix_ms), key, value });
    }
    return [...result.values()];
  }, [display.events]);
  const filter = (key: string, value: string) => { const fields: Record<string, string> = { session_id: 'session', turn_id: 'turn', request_id: 'request', tool_call_id: 'tool', run_id: 'run', activation_id: 'activation', attempt_id: 'attempt' }; setPaused(false); setFollow(false); setPrefs(previous => ({ ...previous, query: `${previous.query} ${fields[key] ?? key}:${queryValue(value)}`.trim() })); };
  async function connect() {
    if (!pairingCode || working) return;
    const code = pairingCode;
    setPairingCode('');
    setWorking(true); setNotice('');
    try {
      const bootstrap = await redeemPairing(code);
      recordingReader.current = null;
      await client.current?.connect(bootstrap);
      setPaused(false); setFollow(true);
    } catch { setNotice('Pairing unavailable or live connection failed. Codes are one-time and expire; use a fresh CLI launcher code to retry, or open a recording. The code was cleared and was not saved.'); }
    finally { setWorking(false); }
  }
  async function openFiles(event: ChangeEvent<HTMLInputElement>) {
    const files = Array.from(event.target.files ?? []);
    event.target.value = '';
    if (!files.length) return;
    if (['connecting', 'live', 'finalizing'].includes(snapshot.state)) { setNotice('Stop observation before replacing the view with a recording. Session execution is unaffected.'); return; }
    setWorking(true); setNotice('');
    try {
      const recording = await openRecording(files);
      recordingReader.current = await createRecordingReader(files);
      setOfflineOffset(0); setOfflineMore(false);
      client.current?.loadRecording(recording.events, recording.issues);
      setSelected(null); setPaused(false); setFollow(false); setOffset(0);
      setNotice('Opened local recording. Search covers only the bounded imported prefix; consult evidence issues for unscanned content and integrity limits.');
    } catch { setNotice('Unable to open recording. Unsupported, corrupt, or incomplete evidence was not silently repaired.'); }
    finally { setWorking(false); }
  }
  async function scanRecording(offset = 0) {
    const reader = recordingReader.current;
    if (!reader) return;
    setWorking(true);
    try {
      const result = await reader.search(offlineText, { offset, limit: WINDOW, maxScanned: 100_000 });
      setPrefs(previous => ({ ...previous, query: '' }));
      client.current?.loadRecording(result.events, result.issues);
      setPaused(false); setFollow(false); setOffset(0); setOfflineOffset(offset); setOfflineMore(result.hasMore);
      setNotice(`Recording text scan: ${result.scanned.toLocaleString()} events scanned, ${result.events.length} returned. ${result.hasMore ? 'More results or unscanned evidence may remain; scan limit is 100,000 events.' : 'Reached scan end; consult evidence issues for integrity and loss.'}`);
    } catch { setNotice('Recording scan failed or exceeded its bounds. No evidence was repaired.'); }
    finally { setWorking(false); }
  }
  function download() {
    if (!window.confirm('Export only the displayed event window? This is NOT a complete recording. Payload bytes, events outside this page, and recording completeness are not included. Sensitive event data may be present.')) return;
    const blob = exportEvents(display.events);
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a'); anchor.href = url; anchor.download = 'xray-event-window.jsonl'; anchor.click();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  }
  const active = ['connecting', 'live', 'finalizing'].includes(snapshot.state);
  return <div className={`app ${prefs.compact ? 'compact' : ''}`}>
    <header className="app-header"><a className="brand" href="#main"><span className="brand-mark">✳</span> X-ray <span className="badge">INSPECTOR</span></a><div className="header-status"><span className={`status-dot ${snapshot.state === 'live' ? 'live' : ''}`} />{snapshot.state}<span className="muted">/ observation only</span></div><div className="header-actions"><form className="pairing-form" onSubmit={event => { event.preventDefault(); void connect(); }}><label htmlFor="pairing-code">CLI pairing code</label><input id="pairing-code" type="password" autoComplete="off" spellCheck={false} autoCapitalize="none" maxLength={4096} value={pairingCode} disabled={working || active} onChange={event => setPairingCode(event.target.value)} placeholder="Paste one-time code" aria-describedby="pairing-help" /><button type="submit" disabled={working || active || !pairingCode}>Connect live</button></form><button disabled={!active || working} onClick={() => { setWorking(true); void client.current?.stop().catch(() => setNotice('Observation cutoff not confirmed; evidence remains incomplete.')).finally(() => setWorking(false)); }}>Stop observation</button><button onClick={() => fileInput.current?.click()} disabled={working}>Open files</button><button onClick={() => directoryInput.current?.click()} disabled={working}>Open folder</button><button onClick={download} disabled={!display.events.length}>Export window ↗</button></div></header>
    <div id="pairing-help" className="pairing-help">Paste the one-time code printed by the local CLI launcher. Memory only; cleared on submission. Never put pairing codes in URLs.</div>
    <input hidden ref={fileInput} type="file" multiple accept=".json,.jsonl,.ndjson,.bin" onChange={event => void openFiles(event)} aria-label="Open recording files" />
    <input hidden ref={directoryInput} type="file" multiple {...{ webkitdirectory: '' }} onChange={event => void openFiles(event)} aria-label="Open recording folder" />
    <div className="toolbar"><div className="searchbox"><span aria-hidden="true">⌕</span><input ref={search} value={prefs.query} onChange={event => { setPaused(false); setFollow(false); setPrefs(previous => ({ ...previous, query: event.target.value })); }} placeholder="Search evidence or filter: domain:provider" aria-label="Search retained evidence" /><kbd>⌘/Ctrl K</kbd>{prefs.query && <button aria-label="Clear search" onClick={() => setPrefs(previous => ({ ...previous, query: '' }))}>×</button>}</div><button onClick={() => setPrefs(previous => ({ ...previous, saved: [...new Set([...previous.saved, previous.query])].filter(Boolean).slice(-12) }))} disabled={!prefs.query}>Save filter</button><button aria-pressed={prefs.compact} onClick={() => setPrefs(previous => ({ ...previous, compact: !previous.compact }))}>Compact</button></div>
    <div className="filterbar"><span className="muted">QUICK FILTERS</span>{['provider', 'tool', 'workflow', 'xray'].map(domain => <button className="chip" key={domain} onClick={() => filter('domain', domain)}>{domain}</button>)}{prefs.saved.map(query => <span className="saved-filter" key={query}><button className="chip" onClick={() => setPrefs(previous => ({ ...previous, query }))}>{query}</button><button aria-label={`Delete saved filter ${query}`} onClick={() => setPrefs(previous => ({ ...previous, saved: previous.saved.filter(item => item !== query) }))}>×</button></span>)}</div>
    {recordingReader.current && <div className="offline-search"><label htmlFor="offline-text">Recording file text scan</label><input id="offline-text" value={offlineText} maxLength={1024} onChange={event => { setOfflineText(event.target.value); setOfflineOffset(0); setOfflineMore(false); }} placeholder="Literal text across recording (not filter syntax)" /><button disabled={working} onClick={() => void scanRecording()}>Scan file</button><button disabled={working || !offlineMore} onClick={() => void scanRecording(offlineOffset + WINDOW)}>Next matches</button><small>Explicit bounded scan · 100,000 events per operation</small></div>}
    {(notice || snapshot.queryError) && <div className="banner" role="status">{snapshot.queryError ? `Search: ${snapshot.queryError}` : notice}<button aria-label="Dismiss notice" onClick={() => setNotice('')}>×</button></div>}
    <main id="main" className="workspace">
      <aside className="navigation"><nav className="tabs" aria-label="Evidence navigation"><button aria-pressed={navigation === 'activities'} onClick={() => setNavigation('activities')}>Activities</button><button aria-pressed={navigation === 'issues'} onClick={() => setNavigation('issues')}>Issues <span className="count">{snapshot.issues.length}</span></button></nav><div className="navigation-body">{navigation === 'activities' ? <><p className="section-label">OBSERVED GROUPS · THIS WINDOW</p>{!groups.length && <p className="muted">Sessions and workflows appear as events arrive.</p>}{groups.map(group => <button className="activity" key={`${group.key}:${group.value}`} onClick={() => filter(group.key, group.value)}><span>{group.domain} <span className="count">{group.count}</span></span><strong className="mono">{group.value}</strong><span className="waterfall"><span style={{ width: `${Math.min(100, Math.max(3, (group.last - group.first) / Math.max(1, ...groups.map(item => item.last - item.first)) * 100))}%` }} /></span><small>{Math.max(0, group.last - group.first).toLocaleString()} ms · observed wall clock</small></button>)}<p className="footnote">Grouped by explicit IDs, not inferred parentage. Wall-clock spans are not execution duration or causal authority.</p></> : <><p className="section-label">EVIDENCE HEALTH</p>{snapshot.issues.length === 0 && <p className="muted">No reported issues. This does not prove complete evidence.</p>}{snapshot.issues.map((issue, index) => <section className="issue" key={`${issue.kind}:${issue.observation_id}:${issue.sequence}:${index}`}><strong>{issue.kind}</strong><p>{issue.message}</p>{issue.sequence !== undefined && <small>Observed sequence #{issue.sequence}</small>}</section>)}</>}</div></aside>
      <section className="event-pane" aria-label="Events"><div className="pane-heading"><h1>Event stream <span className="count">{snapshot.matches.toLocaleString()}</span></h1><div><button aria-pressed={follow} onClick={() => { setFollow(!follow); setPaused(false); }}>↓ Follow</button><button aria-pressed={paused} onClick={() => { setFrozen(snapshot); setPaused(!paused); }}>{paused ? 'Resume view' : 'Pause view'}</button></div></div><div className="coverage"><span>{snapshot.scope === 'opened-recording' ? 'Opened recording search' : 'Retained live search'}</span><span>{paused ? 'View frozen · ingest continues' : 'Bounded event window'}</span></div><div className="event-columns"><span>TIME / SEQ</span><span>DOMAIN</span><span>EVENT / CORRELATION</span></div><div className="event-list" ref={list} tabIndex={0} aria-label="Event rows" onKeyDown={event => { if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return; event.preventDefault(); const current = display.events.findIndex(item => selected && eventId(item) === eventId(selected)); const next = Math.max(0, Math.min(display.events.length - 1, current + (event.key === 'ArrowDown' ? 1 : -1))); setSelected(display.events[next] ?? null); const button = list.current?.querySelectorAll<HTMLButtonElement>('.event-row')[next]; button?.focus(); }}>
      {!display.events.length && <div className="empty"><span className="empty-symbol">◎</span><h2>{snapshot.matches ? 'Loading event window' : 'Your evidence, in focus'}</h2><p>{prefs.query ? 'No matching events in the current search scope.' : 'Connect to a local observation or open a recording to begin.'}</p><p>Live ingest stays bounded. No deep capture is enabled here.</p></div>}{display.events.map(item => <button className={`event-row ${selected && eventId(selected) === eventId(item) ? 'selected' : ''}`} key={eventId(item)} onClick={() => setSelected(item)} aria-pressed={!!selected && eventId(selected) === eventId(item)}><span className="event-time mono">{new Date(item.timestamp_unix_ms).toLocaleTimeString([], { hour12: false })}<small>#{item.sequence}</small></span><span className={`domain domain-${['provider', 'tool', 'workflow', 'xray'].includes(item.event.domain) ? item.event.domain : 'other'}`}>{item.event.domain}</span><span className="event-summary"><strong>{item.event.name}</strong><small className="mono">{item.event.correlation.request_id ?? item.event.correlation.tool_call_id ?? item.event.correlation.run_id ?? item.event.correlation.session_id}{item.event.sensitive ? ' · sensitive' : ''}</small></span></button>)}</div><div className="pagination"><button disabled={paused || offset === 0} onClick={() => { setFollow(false); setOffset(Math.max(0, offset - WINDOW)); }}>← Previous</button><span>{snapshot.matches ? offset + 1 : 0}–{Math.min(offset + display.events.length, snapshot.matches)} of {snapshot.matches.toLocaleString()}</span><button disabled={paused || offset + WINDOW >= snapshot.matches} onClick={() => { setFollow(false); setOffset(offset + WINDOW); }}>Next →</button></div></section>
      <Details key={selected ? eventId(selected) : 'empty'} event={selected} events={display.events} readPayload={async ref => { const value = ref as { payload_id: string; byte_len: number; sha256: string }; if (recordingReader.current) { const bytes = await recordingReader.current.readPayload(value.payload_id); if (bytes.byteLength !== value.byte_len) throw new Error('Payload declaration mismatch'); const hash = await crypto.subtle.digest('SHA-256', Uint8Array.from(bytes).buffer); const digest = Array.from(new Uint8Array(hash), byte => byte.toString(16).padStart(2, '0')).join(''); if (digest !== value.sha256) throw new Error('Payload declaration mismatch'); return bytes; } if (!client.current) throw new Error('offline'); return client.current.readPayload(ref); }} onFilter={filter} evicted={!!selected && !snapshot.events.some(item => eventId(item) === eventId(selected))} />
    </main><footer className="statusbar"><span><b>{snapshot.received.toLocaleString()}</b> received</span><span><b>{snapshot.retained.toLocaleString()}</b> retained · {(snapshot.retainedBytes / 1048576).toFixed(1)} MiB</span><span className={snapshot.evicted ? 'warning' : ''}><b>{snapshot.evicted.toLocaleString()}</b> evicted</span><span>{snapshot.issues.length} evidence issues</span><span className="footer-note">Not canonical history · No execution authority · EOF ≠ completion</span></footer>
  </div>;
}
