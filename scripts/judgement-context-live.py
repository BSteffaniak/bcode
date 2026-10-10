#!/usr/bin/env python3
"""Coordinator-only paired context pilot, using public subject CLI operations.

Command: python3 scripts/judgement-context-live.py --file PILOT.json
No build, daemon startup, credential discovery/reads, or permissions bypass.
Every subject invocation pins TMPDIR=/Volumes/ehdd/bcode-eval-artifacts/tmp;
coordinator daemon startup must use that same TMPDIR (current socket placement).

PILOT schema v1 (all paths absolute): subject_binary, subject_sha256, artifact_id,
pi_root, run_directory (existing empty external directory), profile,
context (nullable), scenario_id, source_order (optional candidate IDs), context_budget_bytes, oracle_module, oracle_sha256, scenarios_sha256,
expected_selection {provider, model, auth_profile}, judgement_selection
{provider_plugin_id, model_id, auth_profile}, and arms (ordered off then on).
Each arm: arm, state_root (distinct existing empty-catalog external state),
config_file, overlay_toml (explicit non-secret TOML),
capture_template (path to a CaptureJob v1). overlay_toml is required non-secret
TOML declaring [judgement] global auto, all six feature modes (only context may
be auto in on), exact [judgement.selection], [session_import] enabled and
[session_import.pi] enabled/custom_only/paths=[pi_root], plus
[model.tool_output] context_chars=context_budget_bytes. Both overlays must be
identical except context_reranking. The coordinator must apply matching overlays
to daemon/session contexts; declarations alone do not certify resolved policy.
Templates supply preregistration, pairing, allowances and configuration identity;
this driver ALWAYS sets configuration_complete=false: it cannot attest all layers.
Both arms use fresh public imports and the same candidate order. A pre-existing
prepared checkpoint is not consumed. Independent oracles stay outside workspaces.
--self-test is offline and spends nothing. Frozen held-out files are never written.
"""
import argparse
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
import uuid
from unittest import mock

# Loading input/oracle modules must not mutate their source directories.
os.sys.dont_write_bytecode = True

EXTERNAL = Path('/Volumes/ehdd/bcode-eval-artifacts')
MAX_BYTES = 2 * 1024 * 1024
SUMMARY = 'Earlier reference material was archived. Implement the new task using admitted evidence only.'


class PilotError(ValueError):
    """Content-free harness precondition failure, never a provider error body."""


def require(condition, reason):
    if not condition:
        raise PilotError(reason)


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def read_json(path):
    with Path(path).open('rb') as stream:
        raw = stream.read(MAX_BYTES + 1)
    require(len(raw) <= MAX_BYTES, 'bounded JSON input exceeded')
    return json.loads(raw)


def write_json(path, value):
    with Path(path).open('x', encoding='utf-8') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')


def external_path(value):
    path = Path(value)
    require(path.is_absolute() and not path.is_symlink(), 'absolute non-symlink path required')
    resolved = path.resolve(strict=True)
    require(resolved.is_relative_to(EXTERNAL.resolve(strict=True)), 'external isolation required')
    return resolved


def history_bindings(page, session_id, candidates):
    """Select exact canonical text, never guess import sequence numbers."""
    require(not page.get('has_more') and not page.get('compatibility_issues'),
            'complete bounded compatible imported history required')
    events = page['events']
    require(0 < len(events) <= 100, 'bounded history required')
    require(all(e['session_id'] == session_id for e in events), 'foreign history')
    require(sum('user_message' in e['kind'] for e in events) == len(candidates)
            and not any('model_turn_started' in e['kind'] or 'model_turn_finished' in e['kind']
                        for e in events), 'imported session not a fresh producer-only checkpoint')
    sequences = [e['sequence'] for e in events]
    require(all(type(s) is int and s >= 0 for s in sequences)
            and sequences == sorted(set(sequences)), 'invalid canonical sequence')
    references = []
    for candidate in candidates:
        matches = [e for e in events if e['kind'].get('user_message', {}).get('text') == candidate['content']]
        require(len(matches) == 1, 'missing or ambiguous exact candidate event')
        require(matches[0]['sequence'] > 0, 'historical reference must have positive canonical sequence')
        references.append({'session_id': session_id, 'sequence': matches[0]['sequence']})
    compactions = [e for e in events if 'context_compacted' in e['kind']]
    require(len(compactions) == 1, 'one imported compaction required')
    compact = compactions[0]['kind']['context_compacted']
    require(compact['summary'] == SUMMARY, 'unexpected or answer-bearing compaction summary')
    require(max(r['sequence'] for r in references) <= compact['compacted_through_sequence']
            < compactions[0]['sequence'], 'import compaction does not cover candidates')
    return references


def pi_records(external_id, workspace, candidates):
    """Only producer inputs and answer-free compaction; no oracle code or tests."""
    stamp = '2026-10-10T12:00:00.000Z'
    records = [{'type': 'session', 'version': 3, 'id': external_id,
                'timestamp': stamp, 'cwd': str(workspace)}]
    parent = None
    for index, candidate in enumerate(candidates):
        entry = f'candidate-{index}'
        records.append({'type': 'message', 'id': entry, 'parentId': parent,
                        'timestamp': stamp, 'message': {'role': 'user',
                        'content': [{'type': 'text', 'text': candidate['content']}]}})
        parent = entry
    records.append({'type': 'compaction', 'id': 'checkpoint', 'parentId': parent,
                    'timestamp': stamp, 'summary': SUMMARY})
    return records


def command(job, arm, args, workspace, timeout=30):
    """Bound output on disk; never include CLI errors/config contents in diagnostics."""
    env = os.environ.copy()
    env['BCODE_CONFIG'] = arm['config_file']
    # Must match coordinator daemon startup: socket placement currently depends on TMPDIR.
    env['TMPDIR'] = str(EXTERNAL / 'tmp')
    # State-root flags do not override an inherited session-store environment pin.
    env['BCODE_STATE_DIR'] = arm['state_root']
    env['BCODE_SESSION_STORE_DIR'] = str(Path(arm['state_root']) / 'sessions')
    # Deliberately replace inherited request-only overrides; never silently inherit.
    env['BCODE_CONFIG_TOML'] = arm['overlay_toml']
    require(not env.get('BCODE_IPC_ENDPOINT'), 'inherited IPC endpoint override unsupported')
    argv = [job['subject_binary'], '--state-root', arm['state_root'], '--profile', job['profile']]
    if job.get('context'):
        argv += ['--context', job['context']]
    argv += args
    with tempfile.TemporaryFile(dir=job['run_directory']) as output:
        try:
            result = subprocess.run(argv, env=env, cwd=workspace, stdin=subprocess.DEVNULL,
                                    stdout=output, stderr=subprocess.DEVNULL, timeout=timeout,
                                    check=False)
        except subprocess.TimeoutExpired:
            raise PilotError('CLI timeout; daemon effects unresolved') from None
        output.seek(0)
        raw = output.read(MAX_BYTES + 1)
    require(result.returncode == 0, 'public CLI operation failed; no effectiveness attribution')
    require(len(raw) <= MAX_BYTES, 'public CLI output exceeded bound')
    return raw.decode('utf-8')


def json_command(job, arm, args, workspace):
    return json.loads(command(job, arm, args, workspace))


def empty_subject_catalog(status):
    """Reject foreign or active state before any public import/mutation."""
    require(status['session_count'] == 0 and status['sessions'] == []
            and status['active_runtime_work'] == [], 'subject catalog/work not empty before fresh import')


def verify_subject(job, arm, workspace):
    require(digest(job['subject_binary']) == job['subject_sha256'], 'subject hash changed')
    status = json_command(job, arm, ['server', 'diagnose', '--json'], workspace)
    empty_subject_catalog(status)
    locations = json_command(job, arm, ['state', 'locations', '--json'], workspace)['locations']
    require(len(locations) == 1 and locations[0]['primary']
            and Path(locations[0]['root']).resolve(strict=True) == Path(arm['state_root']).resolve(strict=True)
            and Path(locations[0]['sessions_root']).resolve(strict=True)
                == (Path(arm['state_root']) / 'sessions').resolve(strict=True),
            'public state/session locations not exclusively isolated')
    daemon = status['daemon']
    require(status['registry_identity_consistent']
            and status['client_executable_digest'] == job['subject_sha256']
            and daemon['executable_digest'] == job['subject_sha256']
            and daemon['artifact_id'] == job['artifact_id']
            and Path(status['client_executable_path']).resolve() == Path(job['subject_binary']).resolve(),
            'exact subject/daemon artifact mismatch')
    require(daemon.get('state_location_id'), 'public state scope missing')
    return {'artifact_id': daemon['artifact_id'], 'executable_sha256': daemon['executable_digest'],
            'state_id': daemon['state_location_id']}


def verify_selection(job, arm, workspace, session_id):
    status = json_command(job, arm, ['model', 'status', session_id, '--json'], workspace)
    expected = job['expected_selection']
    require(status['provider_plugin_id'] == expected['provider']
            and status['effective_model_id'] == expected['model']
            and status['auth_profile'] == expected['auth_profile'], 'effective base selection mismatch')
    return {key: status[key] for key in ('provider_plugin_id', 'effective_model_id', 'auth_profile')}


def validate_overlay(job, arm):
    """Require explicit treatment/root/budget declarations, not resolved-config claims."""
    overlay = tomllib.loads(arm['overlay_toml'])
    judgement = overlay['judgement']
    require(judgement.get('mode') == 'auto', 'explicit global auto required')
    features = ('tool_result_triage', 'context_reranking', 'progress_monitoring',
                'recommendations', 'session_categorization', 'eval_categorization')
    require(all(judgement.get(f) == ('auto' if f == 'context_reranking' and arm['arm'] == 'on' else 'off')
                for f in features), 'explicit isolated context treatment required')
    require(judgement['selection'] == job['judgement_selection'], 'exact Jev overlay pin required')
    pi = overlay['session_import']['pi']
    require(overlay['session_import']['enabled'] and pi['enabled']
            and pi['path_mode'] == 'custom_only'
            and pi['paths'] == [job['pi_root']], 'explicit custom-only Pi root required')
    budget = job['context_budget_bytes']
    require(type(budget) is int and 0 < budget <= 65536
            and overlay['model']['tool_output']['context_chars'] == budget,
            'same bounded framed-context budget required')
    require(Path(arm['config_file']).is_absolute() and Path(arm['config_file']).is_file(),
            'explicit existing base configuration required')


def run(job):
    if os.environ.get('BCODE_IPC_ENDPOINT'):
        require(False, 'inherited IPC endpoint override unsupported')
    require(job['schema_version'] == 1, 'unsupported pilot schema')
    require([a['arm'] for a in job['arms']] == ['off', 'on'], 'exact first pilot off/on pair required')
    run_dir = external_path(job['run_directory'])
    require(not any(run_dir.iterdir()), 'fresh empty pilot directory required')
    external_path(str(EXTERNAL / 'tmp'))
    roots = [external_path(a['state_root']) for a in job['arms']]
    require(roots[0] != roots[1] and not roots[0].is_relative_to(roots[1])
            and not roots[1].is_relative_to(roots[0]), 'distinct non-overlapping arm state roots required')
    for root in roots:
        external_path(str(root / 'sessions'))
    pi_root = external_path(job['pi_root'])
    require(Path(job['subject_binary']).is_absolute(), 'explicit subject binary required')
    oracle = Path(job['oracle_module']).resolve(strict=True)
    scenarios_path = oracle.with_name('context_scenarios.py')
    require(digest(oracle) == job['oracle_sha256']
            and digest(scenarios_path) == job['scenarios_sha256'], 'independent oracle identity mismatch')
    spec = importlib.util.spec_from_file_location('pilot_inputs', scenarios_path)
    scenarios = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(scenarios)
    item = scenarios.scenario(job['scenario_id'])
    candidates_by_id = {c['id']: c for c in item['candidates'] if not c['required']}
    order = job['source_order']
    require(len(order) == len(candidates_by_id) and set(order) == set(candidates_by_id),
            'preregistered exact optional candidate order required')
    candidates = [candidates_by_id[key] for key in order]
    require(sum(len(c['content'].encode()) for c in candidates) > job['context_budget_bytes']
            and all(len(c['content'].encode()) <= job['context_budget_bytes'] for c in candidates),
            'real optional snippet byte pressure required')
    policy = next(c['content'] for c in item['candidates'] if c['required'])
    require(0 < len(candidates) <= 16, 'bounded producer candidates required')
    pin = job['judgement_selection']
    require(pin['provider_plugin_id'] == 'bcode.jev' and pin['model_id'].startswith('jev-')
            and pin['model_id'] != 'jev-' and pin['auth_profile'], 'explicit versioned Jev selection required')
    templates = [read_json(a['capture_template']) for a in job['arms']]
    for arm in job['arms']:
        validate_overlay(job, arm)
    require(job['arms'][0]['config_file'] == job['arms'][1]['config_file'],
            'same explicit base configuration required')
    overlays = [tomllib.loads(a['overlay_toml']) for a in job['arms']]
    for overlay in overlays:
        overlay['judgement'].pop('context_reranking')
    require(overlays[0] == overlays[1], 'paired overlays differ beyond treatment')
    require(templates[0]['request']['capture_id'] != templates[1]['request']['capture_id'],
            'distinct paired capture IDs required')
    require(templates[0]['request']['preregistration_id'] == templates[1]['request']['preregistration_id']
            and templates[0]['request']['repetition'] == templates[1]['request']['repetition'],
            'paired schedule identities differ')
    require(templates[0]['request']['pairing_id'] == templates[1]['request']['pairing_id']
            and templates[0]['request']['task_id'] == templates[1]['request']['task_id'], 'paired identities differ')
    admissions = [copy.deepcopy(t['consumer']['request']['admission']) for t in templates]
    for admission in admissions:
        admission['execution'].pop('historical_context', None)
    require(admissions[0] == admissions[1], 'paired execution allowances differ')
    report = {'schema_version': 1, 'scope': 'development_pilot_not_accuracy_or_promotion',
              'configuration_complete': False,
              'mandatory_omission': 'public_compaction_boundary_plus_runtime_hydration_not_raw_request_capture',
              'framed_installation_bytes': None, 'source_order': order,
              'judgement_selection_declared': pin,
              'oracle_sha256': job['oracle_sha256'], 'scenarios_sha256': job['scenarios_sha256'], 'arms': []}
    sessions = set()
    for arm, template in zip(job['arms'], templates):
        if report['arms'] and report['arms'][-1]['gap'] is not None:
            report['arms'].append({'arm': arm['arm'], 'gap': 'not started after prior arm gap',
                                   'capture': None, 'oracle': None})
            continue
        result = {'arm': arm['arm'], 'gap': None, 'capture': None, 'oracle': None}
        report['arms'].append(result)
        session_id = None
        capture_started = False
        workspace = run_dir / arm['arm']
        workspace.mkdir()
        require(not oracle.is_relative_to(workspace), 'oracle must remain outside subject workspace')
        for name, text in item['starter_files'].items():
            require(Path(name).name == name, 'single starter filename required')
            (workspace / name).write_text(text, encoding='utf-8')
        try:
            identity = verify_subject(job, arm, workspace)
            declared = template['request']['subject']
            require(all(declared.get(k) == identity[k] for k in identity)
                    and declared['executable'] == job['subject_binary'], 'declared subject scope mismatch')
            result['subject'] = identity
            external_id = str(uuid.uuid4())
            project = pi_root / ('judgement-context-' + external_id)
            project.mkdir()
            fixture = project / (external_id + '.jsonl')
            with fixture.open('x', encoding='utf-8') as stream:
                for record in pi_records(external_id, workspace, candidates):
                    stream.write(json.dumps(record) + '\n')
            sources = json_command(job, arm, ['session', 'import', 'sources', '--json'], workspace)
            require(sum(s['source_id'] == 'pi' for s in sources['sources']) == 1, 'missing or ambiguous Pi source')
            discovered = json_command(job, arm, ['session', 'import', 'discover', '--json'], workspace)
            matches = [s for s in discovered['sessions'] if s['source_id'] == 'pi'
                       and s['external_session_id'] == external_id]
            require(len(matches) == 1 and Path(matches[0]['locator']).resolve() == fixture.resolve(),
                    'missing or ambiguous fresh Pi discovery')
            opened = command(job, arm, ['session', 'import', 'open', '--source', 'pi', external_id], workspace)
            session_id = str(uuid.UUID(opened.splitlines()[0]))
            require(session_id not in sessions, 'fresh distinct sessions required')
            sessions.add(session_id)
            result['session_id'] = session_id
            imported_status = json_command(job, arm, ['server', 'diagnose', '--json'], workspace)
            require(imported_status['session_count'] == 1
                    and len(imported_status['sessions']) == 1
                    and imported_status['sessions'][0]['session_id'] == session_id
                    and Path(imported_status['sessions'][0]['working_directory']).resolve() == workspace.resolve()
                    and imported_status['active_runtime_work'] == [],
                    'fresh public catalog does not contain exactly this idle imported session')
            result['selection'] = verify_selection(job, arm, workspace, session_id)
            page = json_command(job, arm, ['session', 'history', session_id, '--limit', '100', '--json'], workspace)
            references = history_bindings(page, session_id, candidates)
            result['history_references'] = references
            capture = copy.deepcopy(template)
            request = capture['request']
            require(capture['schema_version'] == 1 and capture['allow_live']
                    and request['arm'] == arm['arm'] and request['feature'] == 'context_reranking'
                    and request['source'] == 'live_consumer', 'explicit ordinary live context template required')
            request['subject'].update(identity, executable=job['subject_binary'], configuration_complete=False,
                                      provider=job['expected_selection']['provider'],
                                      model=job['expected_selection']['model'],
                                      auth_profile=job['expected_selection']['auth_profile'], model_profile=job['profile'])
            consumer = capture['consumer']
            require(consumer['kind'] == 'normal_turn', 'normal-turn capture required')
            consumer['request'].update(session_id=session_id, prompt=item['prompt'] + '\n' + policy)
            execution = consumer['request']['admission']['execution']
            require(execution.get('permission_mode', 'enforce') == 'enforce', 'normal permissions required')
            execution.update(schema_version=8, historical_context=references,
                             provider_plugin_id=job['expected_selection']['provider'],
                             model_id=job['expected_selection']['model'])
            capture_path = workspace.parent / (arm['arm'] + '-capture-job.json')
            write_json(capture_path, capture)
            result['capture_job_sha256'] = digest(capture_path)
            capture_started = True
            result['capture'] = json.loads(command(job, arm, ['judgement', 'benchmark-capture', '--file',
                                                              str(capture_path)], workspace, timeout=330))
            # No submitted Python is executed: the independent oracle interprets a bounded AST.
            require(digest(oracle) == job['oracle_sha256']
                    and digest(scenarios_path) == job['scenarios_sha256'], 'oracle identity changed')
            oracle_code = ('import sys,json;sys.path.insert(0,sys.argv[1]);import context_oracle;'
                           'print(json.dumps(context_oracle.grade(sys.argv[2],sys.argv[3])))')
            completed = subprocess.run([os.sys.executable, '-I', '-B', '-c', oracle_code, str(oracle.parent),
                                        job['scenario_id'], str(workspace)], stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30, check=False)
            require(completed.returncode == 0 and len(completed.stdout) <= 16384, 'independent oracle unavailable')
            result['oracle'] = json.loads(completed.stdout)
            artifact = workspace / item['artifact']
            require(not artifact.is_symlink(), 'unsafe generated artifact')
            result['artifact_sha256'] = digest(artifact) if artifact.is_file() else None
            captured = result['capture']
            require(captured['scope'] == 'normal_turn' and captured['consumer_executed']
                    and captured.get('output') and captured.get('outcome') == 'succeeded', 'application capture missing; outcome remains unknown')
            selection = captured['output']
            require(selection.get('kind') == 'ranking' and 0 < len(selection['admitted']) < len(candidates),
                    'actual constrained history admission missing')
            if arm['arm'] == 'off':
                telemetry = captured['telemetry']
                require(all(telemetry.get(k) == 0 for k in ('discovery_calls', 'verification_calls', 'inference_calls')),
                        'off optional calls not verified zero')
        except (ValueError, KeyError, OSError, TypeError, subprocess.TimeoutExpired) as error:
            # Keep diagnostics content-free; errors from provider/CLI/module are untrusted.
            result['gap'] = (str(error) if isinstance(error, PilotError) else
                             'pilot unavailable; public response or oracle malformed')
            if session_id and capture_started:
                try:
                    command(job, arm, ['cancel', session_id, '--json'], workspace)
                    result['cancellation_requested'] = True
                except (ValueError, OSError):
                    result['cancellation_requested'] = False
                result['effects_reconciled'] = False
        write_json(run_dir / (arm['arm'] + '-result.json'), result)
    write_json(run_dir / 'pilot.json', report)
    return 0 if all(a['gap'] is None for a in report['arms']) else 1


class OfflineTests(unittest.TestCase):
    def test_external_artifact_root_includes_runs_not_only_tmp(self):
        root = Path('/Volumes/ehdd/bcode-eval-artifacts')
        with mock.patch.object(Path, 'resolve', autospec=True, side_effect=lambda path, **_: path):
            self.assertEqual(external_path(str(root / 'e2e-76b90/pilot1')), root / 'e2e-76b90/pilot1')
            self.assertEqual(external_path(str(root / 'tmp')), root / 'tmp')
            with self.assertRaises(PilotError):
                external_path('/Volumes/ehdd/bcode-eval-artifacts-other/pilot1')
            with self.assertRaises(PilotError):
                external_path('/Users/bsteffaniak/.local/state/bcode')

    def test_exact_canonical_history_and_compaction(self):
        candidates = [{'content': 'archived A'}, {'content': 'archived B'}]
        events = [{'sequence': 0, 'session_id': 's', 'kind': {'session_created': {}}}]
        events.extend({'sequence': i + 3, 'session_id': 's', 'kind': {'user_message': {'text': c['content']}}}
                      for i, c in enumerate(candidates))
        events.append({'sequence': 7, 'session_id': 's', 'kind': {'context_compacted':
                       {'summary': SUMMARY, 'compacted_through_sequence': 4}}})
        page = {'events': events, 'has_more': False, 'compatibility_issues': []}
        self.assertEqual([r['sequence'] for r in history_bindings(page, 's', candidates)], [3, 4])
        for mutate in ('uncovered', 'ambiguous', 'partial', 'foreign', 'leak'):
            broken = copy.deepcopy(page)
            if mutate == 'uncovered':
                broken['events'][-1]['kind']['context_compacted']['compacted_through_sequence'] = 1
            elif mutate == 'ambiguous':
                broken['events'][2]['kind']['user_message']['text'] = 'archived A'
            elif mutate == 'partial':
                broken['has_more'] = True
            elif mutate == 'foreign':
                broken['events'][0]['session_id'] = 'other'
            else:
                broken['events'][-1]['kind']['context_compacted']['summary'] = 'answer'
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                history_bindings(broken, 's', candidates)

    def test_zero_sequence_candidate_is_not_a_valid_history_reference(self):
        page = {'events': [
            {'sequence': 0, 'session_id': 's', 'kind': {'user_message': {'text': 'candidate'}}},
            {'sequence': 1, 'session_id': 's', 'kind': {'context_compacted':
                {'summary': SUMMARY, 'compacted_through_sequence': 0}}}
        ], 'has_more': False, 'compatibility_issues': []}
        with self.assertRaisesRegex(PilotError, 'reference must have positive'):
            history_bindings(page, 's', [{'content': 'candidate'}])

    def test_empty_public_catalog_rejects_parent_sessions_and_work(self):
        empty = {'session_count': 0, 'sessions': [], 'active_runtime_work': []}
        empty_subject_catalog(empty)
        for key, value in (('session_count', 46), ('sessions', [{'session_id': 'parent'}]),
                           ('active_runtime_work', [{'work_id': 'foreign'}])):
            with self.subTest(key=key), self.assertRaises(PilotError):
                empty_subject_catalog(dict(empty, **{key: value}))

    def test_subject_invocation_uses_exact_binary_and_replaces_inherited_overlay(self):
        job = {'subject_binary': '/exact/bcode',
               'profile': 'sol', 'context': 'explicit', 'run_directory': '/external/tmp'}
        arm = {'config_file': '/exact/sol.toml', 'overlay_toml': 'explicit overlay',
               'state_root': '/isolated/state'}
        def launch(argv, **kwargs):
            self.assertEqual(argv, ['/exact/bcode', '--state-root', '/isolated/state',
                                    '--profile', 'sol', '--context', 'explicit', 'server', 'diagnose'])
            self.assertEqual(kwargs['env']['BCODE_CONFIG'], '/exact/sol.toml')
            self.assertEqual(kwargs['env']['BCODE_CONFIG_TOML'], 'explicit overlay')
            self.assertEqual(kwargs['env']['BCODE_STATE_DIR'], '/isolated/state')
            self.assertEqual(kwargs['env']['TMPDIR'], '/Volumes/ehdd/bcode-eval-artifacts/tmp')
            self.assertEqual(kwargs['env']['BCODE_SESSION_STORE_DIR'], '/isolated/state/sessions')
            self.assertEqual(kwargs['cwd'], '/subject/workspace')
            kwargs['stdout'].write(b'{}')
            return subprocess.CompletedProcess(argv, 0)
        with mock.patch.dict(os.environ, {'BCODE_CONFIG_TOML': 'unreproduced inherited overlay',
                                         'BCODE_STATE_DIR': '/live/parent',
                                         'TMPDIR': '/ambient/different-socket-root',
                                         'BCODE_SESSION_STORE_DIR': '/live/parent/sessions',
                                         'BCODE_IPC_ENDPOINT': ''}), \
                mock.patch.object(tempfile, 'TemporaryFile', return_value=io.BytesIO()), \
                mock.patch.object(subprocess, 'run', side_effect=launch):
            self.assertEqual(command(job, arm, ['server', 'diagnose'], '/subject/workspace'), '{}')

    def test_overlay_requires_explicit_isolation_and_jev_pin(self):
        job = {'pi_root': '/external/pi', 'context_budget_bytes': 256,
               'judgement_selection': {'provider_plugin_id': 'bcode.jev',
                                      'model_id': 'jev-1.13.0', 'auth_profile': 'jev'}}
        arm = {'arm': 'on', 'overlay_toml': '[judgement]\nmode="auto"\n'}
        with self.assertRaises(ValueError):
            validate_overlay(job, arm)

    def test_future_or_incomplete_history_never_supplies_references(self):
        for page in ({'has_more': True}, {'compatibility_issues': ['future']}):
            with self.assertRaises(ValueError):
                history_bindings(page, 's', [{'content': 'input'}])

    def test_pi_payload_has_only_inputs_and_answer_free_summary(self):
        records = pi_records('fresh', '/external/subject', [{'content': 'quoted contract'}])
        self.assertEqual(records[0]['version'], 3)
        self.assertEqual(records[1]['message']['content'][0]['text'], 'quoted contract')
        self.assertEqual(records[-1]['summary'], SUMMARY)
        self.assertEqual(records[-1]['parentId'], records[1]['id'])


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--file', type=Path)
    group.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(OfflineTests)
        return 0 if unittest.TextTestRunner().run(suite).wasSuccessful() else 1
    try:
        return run(read_json(args.file))
    except (ValueError, KeyError, OSError, TypeError, ImportError):
        parser.exit(1, 'Pilot precondition failed; no configuration/effectiveness attestation.\n')


if __name__ == '__main__':
    raise SystemExit(main())
