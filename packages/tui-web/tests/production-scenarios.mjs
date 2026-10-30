import { committedCheckpoint } from '/production-evidence.mjs';
const assert = (value, message) => { if (!value) throw new Error(message); };
const pause = () => new Promise(resolve => setTimeout(resolve, 35));

// Every producer here is an actual registered backend plugin dispatched by the
// deterministic provider. No fixture inserts session events or manufactures UI.
export async function runProductionScenarios({configuration, wasm, mountInteractiveSurface, mountBrowserSession}) {
  const results = [];
  for (const scenario of configuration.scenarios) {
    // The app deliberately retains at most eight consumers for 60 seconds,
    // including the page's initial session. Finish each bounded batch before
    // admitting more; disposal does not revoke a server-owned lease.
    if (results.length > 0 && results.length % 7 === 0) {
      await new Promise(resolve => setTimeout(resolve, 61000));
    }
    let mounted, regions = [], observed, errors = [], controls = [], copied = [], navigated = [];
    let clicks = 0, downloadHash, overbudgetError, overbudgetBytes;
    const recoveryState = {};
    const canvas = document.getElementById('canvas');
    const snapshotPath = `/api/session-view/${scenario.session}`;
    const api = async (path, body, token = configuration.token) => {
      const response = await fetch(`${path}${path.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}`, body === undefined ? {} : {
        method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify(body),
      });
      return response;
    };
    const snapshot = async () => {
      const deadline = performance.now() + 30000;
      let next;
      do {
        next = await (await api(snapshotPath)).json();
        if (next.error !== 'browser_busy') break;
        // This independent, read-only verifier shares admission with mounted
        // polling. Retry only explicit non-admission, never a control operation.
        assert(performance.now() < deadline, `${scenario.kind}: snapshot admission remained busy`);
        await pause();
      } while (true);
      observed = next;
      assert(observed.snapshot, `${scenario.kind}: missing canonical snapshot ${JSON.stringify(observed)}`);
      return observed.snapshot;
    };
    const nativeBinding = (scope, action) => {
      const bindings = observed?.policy?.bindings;
      assert(Array.isArray(bindings), 'native exported bindings missing');
      const binding = bindings.find(binding => binding.scope === scope && binding.binding?.type === 'host' && binding.binding.action === action);
      assert(binding?.chord?.length, `native binding missing ${scope}/${action}`);
      return binding;
    };
    const invokeNative = async (scope, action) => {
      await snapshot();
        const native = nativeBinding(scope, action);
      for (let tap = 0; tap < (native.multi_tap?.taps ?? 1); tap++) {
        for (const stroke of native.chord) {
          key(stroke.key === 'escape' ? 'Escape' : stroke.key === 'enter' ? 'Enter' : stroke.key,
            {ctrlKey:stroke.ctrl, altKey:stroke.alt, shiftKey:stroke.shift, metaKey:stroke.meta});
          await pause();
        }
      }
    };
    const wait = async (predicate, label) => {
      const deadline = performance.now() + (scenario.kind === 'artifact' ? 330000 : 30000);
      while (true) {
        const value = await predicate();
        if (value) return value;
        if (performance.now() > deadline) throw new Error(`${scenario.kind}: timeout ${label}; ${JSON.stringify(observed && {revision: observed.revision, runtime: observed.snapshot.runtime, tools: Object.values(observed.snapshot.tools).map(tool => ({id:tool.tool_call_id,status:tool.status,has_text:tool.result_text !== null})), permissions: observed.snapshot.permissions, snapshot: observed.snapshot}).slice(0,10000)}; errors=${errors}`);
        await pause();
      }
    };
    const checkpoint = async (label, activation = null) => {
      await mounted.refresh();
      const currentRegions = regions;
      return committedCheckpoint(`${scenario.kind}-${label}`, canvas, currentRegions,
        activation && { ...activation, cellWidth:9, cellHeight:18 });
    };
    const key = (key, options = {}) => {
      const editor = document.querySelector('[data-bmux-input]');
      assert(editor, `${scenario.kind}: input bridge missing`);
      editor.dispatchEvent(new KeyboardEvent('keydown', {key, code:key.length === 1 ? `Key${key.toUpperCase()}` : key,
        bubbles:true, cancelable:true, ...options}));
    };
    const guard = () => ({session_id:scenario.session, epoch:observed.epoch, revision:observed.revision});
    const reject = async (operation, expected = 'stale_control') => {
      const response = await (await api('/api/session-view/control', operation)).json();
      assert(response.error === expected, `${scenario.kind}: expected ${expected}, got ${JSON.stringify(response)}`);
    };
    const rustDiagnostics = [];
    const originalConsoleError = console.error;
    console.error = (...values) => {
      if (rustDiagnostics.length < 8) rustDiagnostics.push(values.map(String).join(' ').slice(0, 2000));
      originalConsoleError(...values);
    };
    const observedWasm = {...wasm, BrowserSession: class extends wasm.BrowserSession {
      input(json) {
        try { return super.input(json); }
        catch (error) { errors.push(`Rust input: ${String(error)}`); throw error; }
      }
    }};
    const mount = () => mountBrowserSession({
      wasm: observedWasm, mountInteractiveSurface, container:document.getElementById('host'), canvas,
      sessionId:scenario.session, token:configuration.token, backendOrigin:location.origin,
      pollMs:60000, requestTimeoutMs:60000, recoveryState,
      bindings:{},
      onSemantics:value => { regions = value; }, onError:error => errors.push(String(error)),
      onBackendResult:result => { if (result.type === 'control') controls.push(JSON.parse(result.json)); },
      capabilities:{writeClipboard:text => { copied.push(text); }, navigate:url => { navigated.push(url); }},
    });
    try {
      await snapshot();
      mounted = await mount();
      const initialState = await snapshot();
      const initialSequence = initialState.latest_sequence;
      mounted.focus();
      const editor = document.querySelector('[data-bmux-input]');
      editor.value = scenario.prompt;
      editor.dispatchEvent(new InputEvent('input', {inputType:'insertText', data:scenario.prompt, bubbles:true}));
      await pause();
      if (scenario.kind === 'config') {
        key('Enter');
        await pause();
        const state = await snapshot();
        assert(!state.runtime.active_turn_id && state.latest_sequence === initialSequence,
          'configured Enter submitted instead of inserting newline');
        await checkpoint('newline');
        await pause();
        key('Enter', {ctrlKey:true});
      } else key('Enter', {ctrlKey:true});
      // Drain the mounted adapter's serialized input/submission work before
      // issuing independent observations that share the host admission gate.
      await mounted.refresh();
      if (scenario.kind.startsWith('permission-') || scenario.kind.startsWith('batch-')) {
        const batch = scenario.kind.startsWith('batch-');
        const approved = scenario.kind.endsWith('approve');
        const pending = await wait(async () => {
          const state = await snapshot();
          const pending = state.permissions.filter(value => !value.resolved);
          return pending.length === (batch ? 2 : 1) && pending;
        }, 'real permission admission');
        if (batch) assert(pending[0].batch?.batch_id === pending[1].batch?.batch_id && pending[0].batch?.call_count === 2,
          'permission calls did not form actual batch');
        const grouped = pending[0].batch != null;
        const action = grouped ? {type:'permission_batch', batch_id:pending[0].batch.batch_id, approved}
          : {type:'permission', permission_id:pending[0].permission_id, approved, remember:false};
        const stale = {...guard(), action};
        // Approval must come from the actual shared BMUX permission controls.
        const label = grouped ? (approved ? 'approve batch' : 'deny batch') : (approved ? 'approve' : 'deny');
        await checkpoint('pending');
        const target = regions.find(region => !region.disabled && (region.text === label || region.label === label
          || region.description === label));
        assert(target, `committed permission target missing: ${label}; ${JSON.stringify(regions)}`);
        await checkpoint('decision', {id:target.id});
        clicks++;
        await wait(async () => {
          await mounted.refresh();
          const state = await snapshot();
          const actual = state.permissions.filter(value => pending.some(previous => previous.permission_id === value.permission_id));
          const outcome = controls.some(response => response.outcome?.type === (grouped ? 'permission_batch_resolved' : 'permission_resolved')
            && (grouped ? response.outcome.resolved_count === pending.length : response.outcome.resolved === true));
          return actual.every(value => value.resolved && value.approved === approved) && outcome;
        }, 'canonical permission resolution');
        await reject(stale, 'stale_observation');
        await snapshot();
        await reject({...guard(), action});
      } else if (scenario.kind.startsWith('question-')) {
        const interaction = await wait(async () => (await snapshot()).interactions.find(value => !value.resolved), 'actual question tool exchange');
        assert(interaction.producer_id === 'bcode.question', 'question not owned by registered producer');
        await checkpoint('question');
        if (scenario.kind === 'question-option') {
          assert(interaction.snapshot?.request?.questions?.[0]?.options?.length === 2,
            `option fixture canonical request differs: ${JSON.stringify(interaction.snapshot)}`);
          const identity = interaction.interaction_id;
          await mounted.dispose();
          mounted = await mount();
          await checkpoint('pending-reconnect');
          const reconnected = (await snapshot()).interactions.filter(value => !value.resolved);
          assert(reconnected.length === 1 && reconnected[0].interaction_id === identity, 'nonterminal reconnect duplicated or lost canonical question');
        }
        const stale = {...guard(), action:{type:'interaction', interaction_id:interaction.interaction_id,
          input:{type:'submit', value:{answers:[]}}}};
        if (scenario.kind === 'question-option') {
          await mounted.refresh();
          if (!regions.some(region => region.id === 'question-0.option-1' && !region.disabled)) {
            const current = await snapshot();
            throw new Error(`option lost after refresh: ${JSON.stringify({interactions:current.interactions,
              runtime:current.runtime, transcript:current.transcript.items.slice(-5)}).slice(0,18000)}`);
          }
          await checkpoint('select-second', {id:'question-0.option-1'});
          clicks++;
          await wait(async () => { await mounted.refresh(); const current = (await snapshot()).interactions.find(value => value.interaction_id === interaction.interaction_id);
            return current?.snapshot?.selected_option_indices?.[0]?.includes(1); }, 'second option acknowledged');
          const submit = regions.find(region => !region.disabled && (region.text === 'Submit' || region.label === 'Submit'));
          assert(submit, 'question Submit missing committed control');
          await checkpoint('submit', {id:submit.id});
          clicks++;
        } else if (scenario.kind === 'question-custom') {
          const custom = regions.find(region => !region.disabled && region.role === 'text-input'
            && region.id.startsWith('question.answer.'));
          assert(custom, 'question custom editor missing committed geometry');
          await checkpoint('focus-custom', {id:custom.id});
          clicks++;
          await wait(async () => { await mounted.refresh(); const current = (await snapshot()).interactions.find(value => value.interaction_id === interaction.interaction_id);
            return current?.snapshot?.focused_control_id === 'question-0.custom'; }, 'canonical custom focus');
          const input = document.querySelector('[data-bmux-input]');
          input.value = 'browser custom 日本語 🦀 é';
          input.dispatchEvent(new InputEvent('input', {inputType:'insertText', data:input.value, bubbles:true}));
          await wait(async () => { await mounted.refresh(); const current = (await snapshot()).interactions.find(value => value.interaction_id === interaction.interaction_id);
            return current?.snapshot?.answers?.[0]?.custom === 'browser custom 日本語 🦀 é'; }, 'canonical custom text');
          await checkpoint('custom');
          const submitBinding = nativeBinding('question', 'question.submit');
          assert(submitBinding.chord.length === 1 && submitBinding.chord[0].key === 'Enter'
            && submitBinding.chord[0].ctrl, 'fixture requires effective question Ctrl+Enter submit');
          key('Enter', {ctrlKey:true});
          await mounted.refresh();
        } else key('Escape');
        const terminal = await wait(async () => { await mounted.refresh(); return (await snapshot()).interactions.find(value => value.interaction_id === interaction.interaction_id && value.resolved); }, 'canonical question terminal resolution');
        const payload = terminal.resolution?.payload;
        if (scenario.kind === 'question-option') assert(payload?.questions?.[0]?.selected?.includes('second'), 'canonical second option differs');
        if (scenario.kind === 'question-custom') assert(payload?.questions?.[0]?.custom === 'browser custom 日本語 🦀 é', 'canonical custom answer differs');
        if (scenario.kind === 'question-cancel') assert(JSON.stringify(terminal.resolution).includes('dismissed'), 'canonical dismissal differs');
        await reject(stale, 'stale_observation');
        await snapshot();
        await reject({...guard(), action:stale.action});
      } else if (scenario.kind === 'artifact') {
        const pending = await wait(async () => {
          await mounted.refresh();
          const state = await snapshot();
          return state.permissions.find(value => !value.resolved && value.tool_name === 'shell.run');
        }, 'artifact shell permission admission');
        const grouped = pending.batch != null;
        const label = grouped ? 'approve batch' : 'approve';
        await checkpoint('shell-pending');
        const target = regions.find(region => !region.disabled &&
          [region.text, region.label, region.description].includes(label));
        assert(target, 'artifact shell approval missing committed producer control');
        await checkpoint('shell-approve', {id:target.id});
        clicks++;
        await wait(async () => {
          await mounted.refresh();
          const state = await snapshot();
          const outcome = controls.some(response =>
            response.outcome?.type === (grouped ? 'permission_batch_resolved' : 'permission_resolved')
            && (grouped ? response.outcome.resolved_count === 1 : response.outcome.resolved === true));
          return outcome && state.permissions.every(value =>
            value.permission_id !== pending.permission_id || (value.resolved && value.approved === true));
        }, 'artifact shell permission resolution');
      } else if (scenario.kind === 'cancel') {
        await wait(async () => (await snapshot()).runtime.active_turn_id, 'active cancellable provider turn');
        await checkpoint('streaming');
        const operation = {...guard(), action:{type:'cancel_turn', turn_id:observed.snapshot.runtime.active_turn_id}};
        await invokeNative('chat', 'app.interrupt');
        await wait(async () => controls.some(response => response.outcome?.type === 'cancelled' && response.outcome.cancelled), 'configured native cancel binding');
        await wait(async () => !(await snapshot()).runtime.active_turn_id, 'canonical cancellation terminal');
        await reject(operation, 'stale_observation');
      }
      await wait(async () => {
        const state = await snapshot();
        const tools = Object.values(state.tools);
        if (scenario.kind === 'config') return !state.runtime.active_turn_id && JSON.stringify(state.transcript).includes(`fake: ${scenario.prompt}`);
        if (scenario.kind === 'rich') return !state.runtime.active_turn_id && JSON.stringify(state.transcript).includes('let browser = true;');
        if (scenario.kind === 'cancel') return !state.runtime.active_turn_id && controls.some(response => response.outcome?.type === 'cancelled');
        const expected = scenario.kind.startsWith('batch-') || scenario.kind === 'artifact' ? 2 : 1;
        if (scenario.kind === 'artifact' && !state.runtime.active_turn_id) {
          assert(state.runtime.last_turn_outcome === 'completed',
            `artifact terminal failure: ${JSON.stringify({runtime:state.runtime, tools:tools.map(tool => ({id:tool.tool_call_id,status:tool.status,has_text:tool.result_text !== null})), submissions:state.submissions})}`);
        }
        return !state.runtime.active_turn_id && tools.length === expected && tools.every(tool =>
          ['finished','failed','cancelled'].includes(tool.status) && tool.result_text !== null);
      }, 'canonical turn finished');
      await checkpoint('terminal');
      if (scenario.kind === 'permission-approve' || scenario.kind === 'batch-approve') {
        const producerAction = regions.find(region => !region.disabled && region.id.startsWith('transcript-action:'));
        assert(producerAction, `filesystem producer supplied no committed local action: ${JSON.stringify(regions)}`);
        const before = await checkpoint('producer-before');
        const sequence = observed.snapshot.latest_sequence;
        await checkpoint('producer-action', {id:producerAction.id});
        const after = await checkpoint('producer-after');
        assert(before.pixel_sha256 !== after.pixel_sha256 || copied.length > 0, 'producer local action had no visible or clipboard result');
        assert((await snapshot()).latest_sequence === sequence, 'producer presentation action mutated canonical session');
      }
      if (scenario.kind === 'rich') {
        const canonicalSequence = observed.snapshot.latest_sequence;
        const disclosure = regions.find(region => !region.disabled && region.id.startsWith('markdown:')
          && (region.text?.includes('Expand detail') || region.label?.includes('Expand detail') || region.description?.includes('details')));
        assert(disclosure, 'rich disclosure missing committed action region');
        const closed = await checkpoint('disclosure-before');
        await checkpoint('disclosure-activate', {id:disclosure.id});
        const opened = await checkpoint('disclosure-after');
        assert(closed.pixel_sha256 !== opened.pixel_sha256, 'local disclosure did not change committed pixels');
        assert((await snapshot()).latest_sequence === canonicalSequence, 'local disclosure mutated canonical session');
        const copy = regions.find(region => !region.disabled && region.id.startsWith('transcript-copy:'));
        assert(copy, `rich code-block copy has no committed hit: ${JSON.stringify(regions)}`);
        await checkpoint('copy', {id:copy.id});
        await wait(() => copied.some(text => text.includes('let browser = true;')), 'real code-block clipboard action');
        const link = regions.find(region => !region.disabled && region.id.startsWith('markdown:') && region.id !== disclosure.id);
        assert(link, 'rich link has no committed hit');
        await checkpoint('link', {id:link.id});
        await wait(() => navigated.includes('https://example.invalid/browser-acceptance'), 'real link navigation capability');
      }
      if (scenario.kind === 'artifact') {
        const item = observed.snapshot.transcript.items.find(item => item.kind?.tool?.result?.artifact?.artifact?.refs?.some(ref => ref.key === 'inline-image'));
        assert(item, 'real multi-range artifact missing');
        const first = await mounted.readArtifact(item.id, 'inline-image', 0n, 131072);
        const metadata = JSON.parse(first.json);
        assert(metadata.total_bytes > 131072 && first.bytes.length === 131072 && metadata.finalized, 'artifact does not cross canonical range boundary');
        await snapshot();
        const request = {...guard(), item_id:item.id, reference_key:'inline-image', offset:0, length:131072};
        const response = await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify(request))}`);
        assert(response.headers.get('content-type')?.startsWith('application/octet-stream'), 'multi-range download not binary');
        const bytes = new Uint8Array(await response.arrayBuffer());
        assert(bytes.length === metadata.total_bytes && JSON.stringify([...bytes.slice(0,131072)]) === JSON.stringify([...first.bytes]), 'multi-range download incomplete or differs');
        downloadHash = [...new Uint8Array(await crypto.subtle.digest('SHA-256',bytes))].map(byte => byte.toString(16).padStart(2,'0')).join('');
        const second = await mounted.readArtifact(item.id, 'inline-image', 131072n, 131072);
        assert(JSON.stringify([...bytes.slice(131072)]) === JSON.stringify([...second.bytes]), 'complete download tail differs from canonical second range');
        const negative = await (await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify({...request,length:64*1024*1024+1}))}`)).json();
        assert(negative.error === 'invalid_artifact_range', 'unbounded download request accepted');
        const oversized = observed.snapshot.transcript.items.find(item => item.kind?.tool?.result?.artifact?.artifact?.refs?.some(ref => ref.key === 'clean_output' && ref.byte_len > 64*1024*1024));
        assert(oversized, 'real shell producer did not retain oversized canonical clean_output');
        const oversizedRef = oversized.kind.tool.result.artifact.artifact.refs.find(ref => ref.key === 'clean_output');
        const oversizedRequest = {...guard(),item_id:oversized.id,reference_key:'clean_output',offset:0,length:131072};
        const probe = await (await api('/api/session-view/artifact', oversizedRequest)).json();
        assert(probe.finalized && probe.total_bytes > 64*1024*1024 && probe.bytes?.length === 131072
          && probe.reference_key === 'clean_output' && probe.artifact_id === oversized.kind.tool.result.artifact.artifact.artifact_id,
          'oversized artifact metadata/first canonical range not established');
        overbudgetBytes = probe.total_bytes;
        assert(overbudgetBytes === oversizedRef.byte_len, 'canonical oversized reference/retained length mismatch');
        const rejectedDownload = await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify(oversizedRequest))}`);
        assert(rejectedDownload.headers.get('content-type')?.startsWith('application/json'), 'oversized download exposed successful octetstream');
        const rejection = await rejectedDownload.json();
        overbudgetError = rejection.error;
        assert(overbudgetError === 'artifact_download_exceeds_budget', `wrong total-size rejection: ${JSON.stringify(rejection)}`);
        await checkpoint('overbudget-rejected');
      }
      if (scenario.kind === 'image') {
        const item = observed.snapshot.transcript.items.find(item => item.kind?.tool?.result?.artifact?.artifact?.refs?.some(ref => ref.key === 'inline-image'));
        assert(item, `real filesystem image artifact absent: ${JSON.stringify(observed.snapshot.tools)}`);
        const itemId = item.id;
        const before = await checkpoint('before-image');
        assert(await mounted.showArtifactImage(itemId, 'inline-image'), 'canonical image rejected by retained compositor');
        const after = await committedCheckpoint('image-committed', canvas, regions);
        assert(before.pixel_sha256 !== after.pixel_sha256, 'decoded artifact image never changed destination pixels');
        const beforeRange = await mounted.readArtifact(itemId, 'inline-image', 0n, 131072);
        assert(beforeRange.bytes.length > 0, 'production range client returned no bytes');
        await snapshot();
        const request = {...guard(), item_id:itemId, reference_key:'inline-image', offset:0, length:131072};
        const range = await (await api('/api/session-view/artifact', request)).json();
        assert(range.reference_key === request.reference_key && range.artifact_id === item.kind.tool.result.artifact.artifact.artifact_id,
          'range did not preserve canonical artifact/reference identity');
        assert(range.finalized && range.complete !== false && range.total_bytes === range.bytes.length && range.bytes.length > 0, 'canonical image range incomplete');
        const download = await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify(request))}`);
        assert(download.headers.get('content-type')?.startsWith('application/octet-stream'), 'download is not complete binary');
        const bytes = new Uint8Array(await download.arrayBuffer());
        assert(JSON.stringify([...bytes]) === JSON.stringify(range.bytes), 'complete download differs from canonical range');
        for (const [mutation, expected] of [
          [{epoch:'00000000-0000-4000-8000-000000000000'}, 'stale_observation'],
          [{item_id:'never-issued-artifact'}, 'stale_artifact'],
          [{reference_key:'never-issued-reference'}, 'stale_artifact'],
          [{length:131073}, 'invalid_artifact_range'], [{length:0}, 'invalid_artifact_range'],
        ]) {
          const response = await (await api('/api/session-view/artifact', {...request, ...mutation})).json();
          assert(response.error === expected, `artifact boundary: ${JSON.stringify(response)}`);
        }
        const unauthorized = await (await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify(request))}`, undefined, 'wrong-token')).json();
        assert(unauthorized.error === 'unauthorized', 'unauthorized artifact download disclosed bytes');
        const offset = await (await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify({...request,offset:1}))}`)).json();
        assert(offset.error === 'invalid_artifact_range', 'partial successful download accepted');
        for (const [mutation, expected] of [
          [{revision:0}, 'stale_observation'],
          [{item_id:'never-issued-artifact'}, 'stale_artifact'],
          [{reference_key:'never-issued-reference'}, 'stale_artifact'],
          [{length:131073}, 'invalid_artifact_range'],
          [{length:0}, 'invalid_artifact_range'],
        ]) {
          const rejected = await (await api(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify({...request,...mutation}))}`)).json();
          assert(rejected.error === expected, `download boundary expected ${expected}: ${JSON.stringify(rejected)}`);
        }
        const foreign = await fetch(`/api/session-view/artifact/download?request=${encodeURIComponent(JSON.stringify(request))}&token=${encodeURIComponent(configuration.token)}`, {
          method:'POST', headers:{'content-type':'application/json'}, body:'{}',
        });
        const foreignResult = await foreign.json();
        assert(foreignResult.error === 'method_not_allowed', 'artifact download admitted non-GET request');
      }
      // A fresh bridge/WASM instance retains no observation or hit cache. It must
      // reconstruct truthful pixels through the same production application API.
      await mounted.dispose();
      mounted = await mount();
      const remount = await checkpoint('reconnect');
      assert(remount.ink > 0, 'reconnect lost actual UI');
      assert(!JSON.stringify(await snapshot()).includes('FORBIDDEN LATE PROVIDER OUTPUT'), 'terminal turn reopened by stale output');
      results.push({kind:scenario.kind, passed:true, clicks, copied, navigated, download_sha256:downloadHash, overbudget_download_error:overbudgetError, overbudget_total_bytes:overbudgetBytes, terminal_sequence:observed.snapshot.latest_sequence});
    } catch (error) {
      try { await mounted?.dispose(); } catch { /* retain the initiating acceptance failure */ }
      mounted = null;
      const causes = [];
      for (let cause = error; cause && causes.length < 6; cause = cause.cause) causes.push(String(cause));
      throw new Error(`${causes.join(' caused by ')}; prior errors=${errors.join('; ')}; Rust diagnostics=${rustDiagnostics.join('; ')}`);
    } finally {
      console.error = originalConsoleError;
      await mounted?.dispose();
    }
  }
  return results;
}
